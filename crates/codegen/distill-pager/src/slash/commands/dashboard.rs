//! `/dashboard`: open the Agent Dashboard view.
//!
//! The dashboard shows every running session, top-level and subagents, with peek, attach, and dispatch from one screen.
//! Attaching to a subagent reuses the existing fullscreen takeover, so attaching never bypasses `active_subagent`.
//!
//! Like other session-less commands it runs through `Action` only and takes no args.
//! The command is hidden in the registry until `dashboard_enabled()` reveals it via [`crate::app::agent_view::AgentView::set_dashboard_visible`].
//! With `[dashboard].enabled = false` or `GROK_AGENT_DASHBOARD=0` the dispatcher shows a toast and refuses to open.
//! The dashboard is independent of leader mode.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};
use crate::slash::{ModeSupport, Remedy};

pub struct DashboardCommand;

impl SlashCommand for DashboardCommand {
    slash_meta! {
        name: "dashboard",
        // `/sessions` stays as an alias after the sessions picker modal was removed, so old muscle memory redirects here
        // The dashboard replaced that modal for switching, renaming, and closing active sessions.
        // The aliases inherit the feature gate (`set_dashboard_visible` hides by canonical name) and the minimal-mode gate below
        aliases: ["agents-dashboard", "sessions"],
        description: "Open the Agent Dashboard",
        usage: "/dashboard",
        mode_support: ModeSupport::FullscreenOnly(Remedy::SwitchMode {
            why: "minimal is single-session",
        }),
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenDashboard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::slash::command::{CommandExecCtx, CommandResult};

    #[test]
    fn run_returns_open_dashboard_action() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        };
        let cmd = DashboardCommand;
        assert!(matches!(
            cmd.run(&mut ctx, ""),
            CommandResult::Action(Action::OpenDashboard)
        ));
    }

    #[test]
    fn does_not_take_args() {
        let cmd = DashboardCommand;
        assert!(!cmd.takes_args());
    }

    /// `/sessions` (the removed picker modal) and `/agents-dashboard` both spell this command.
    #[test]
    fn aliases_include_sessions() {
        let cmd = DashboardCommand;
        assert_eq!(cmd.aliases(), &["agents-dashboard", "sessions"]);
    }
}
