// Modified for Distill by Samuel Fajreldines, 2026.
//! `/hooks` and `/plugins`: open the hooks/plugins modal.
//!
//! These commands always open the tabbed modal.
//! All hook/plugin management (install, uninstall, trust, etc.) is done through the modal's UI; no subcommands are passed through to the shell.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};
use crate::views::extensions_modal::ExtensionsTab;
use distill_telemetry::events::ExtensionsModalTrigger;

/// Open the hooks/plugins modal on the Hooks tab.
pub struct HooksCommand;

impl SlashCommand for HooksCommand {
    slash_meta! {
        name: "hooks",
        description: "View hooks",
        usage: "/hooks",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Hooks,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}

/// Open the hooks/plugins modal on the Plugins tab.
pub struct PluginsCommand;

impl SlashCommand for PluginsCommand {
    slash_meta! {
        name: "plugins",
        aliases: ["plugin"],
        description: "View plugins",
        usage: "/plugins",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Plugins,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}

/// Open the hooks/plugins modal on the Marketplace tab.
pub struct MarketplaceCommand;

impl SlashCommand for MarketplaceCommand {
    slash_meta! {
        name: "marketplace",
        description: "View marketplace",
        usage: "/marketplace",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Marketplace,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}

/// Open the hooks/plugins modal on the Skills tab.
pub struct SkillsCommand;

impl SlashCommand for SkillsCommand {
    slash_meta! {
        name: "skills",
        description: "View skills",
        usage: "/skills",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Skills,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}
