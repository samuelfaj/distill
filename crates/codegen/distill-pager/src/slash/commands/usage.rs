// Modified for Distill by Samuel Fajreldines, 2026.
//! `/usage` shows session token and cost totals; consumer accounts can also manage billing.
//!
//! External-auth deployments (`auth_provider_command`) never reach grok.com billing.
//! The command stays discoverable even when an individual provider is
//! disconnected; only Grok's manage/billing surface is separately guarded.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use agent_client_protocol as acp;

pub struct UsageCommand;

/// Detect external-auth installs once at pager startup.
pub(crate) fn detect_external_auth_provider(auth_methods: &[acp::AuthMethod]) -> bool {
    auth_methods.iter().any(auth_method_is_external_provider)
        || auth_provider_env_set()
        || auth_provider_config_set()
}

fn auth_method_is_external_provider(method: &acp::AuthMethod) -> bool {
    method
        .meta()
        .as_ref()
        .and_then(|v| v.get("external_provider"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn auth_provider_env_set() -> bool {
    std::env::var("GROK_AUTH_PROVIDER_COMMAND")
        .ok()
        .is_some_and(|s| !s.trim().is_empty())
}

fn auth_provider_config_set() -> bool {
    let Ok(raw) = distill_shell::config::load_effective_config() else {
        return false;
    };
    let Ok(cfg) = distill_shell::agent::config::Config::new_from_toml_cfg(&raw) else {
        return false;
    };
    cfg.grok_com_config
        .auth_provider_command
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
}

impl SlashCommand for UsageCommand {
    slash_meta! {
        name: "usage",
        aliases: ["cost"],
        description: "View usage",
        usage: "/usage [show|manage]",
        takes_args: true,
    }

    fn visible(&self, ctx: &AppCtx) -> bool {
        ctx.usage_command_visible
    }

    fn takes_args_now(&self, ctx: &AppCtx) -> bool {
        ctx.usage_command_visible
    }

    fn suggest_args(&self, ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        if !ctx.usage_command_visible {
            return None;
        }
        let mut args = vec![ArgItem {
            display: "show".into(),
            match_text: "show".into(),
            insert_text: "show".into(),
            description: "View usage".into(),
        }];
        if ctx.billing_surface_visible {
            args.push(ArgItem {
                display: "manage".into(),
                match_text: "manage".into(),
                insert_text: "manage".into(),
                description: "Manage billing".into(),
            });
        }
        Some(args)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if !ctx.usage_command_visible {
            return CommandResult::Error("/usage is not available.".into());
        }
        let arg = args.trim();
        match arg {
            "" | "show" => CommandResult::Action(Action::ShowUsage),
            "manage" if ctx.billing_surface_visible => CommandResult::Action(Action::ManageBilling),
            "manage" => {
                CommandResult::Error("/usage manage is only available for Grok billing.".into())
            }
            _ => CommandResult::Error(format!(
                "Unknown argument: {arg}. Use /usage show or /usage manage"
            )),
        }
    }
}
