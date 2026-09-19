//! `/cd [path]`: change the working directory new dashboard sessions spawn in.
//!
//! With no argument it opens the dashboard's location picker; with a path it changes directly.
//! Both only work on the dashboard: anywhere else the dispatcher prints a toast pointing the user at `/dashboard`.
//! See `dispatch_dashboard_open_location_picker` and `dispatch_dashboard_change_location`.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct CdCommand;

impl SlashCommand for CdCommand {
    slash_meta! {
        name: "cd",
        description: "Change the working directory for new agents",
        usage: "/cd [path]",
        takes_args: true,
        // `/cd` only makes sense on the dashboard (it changes where the dashboard dispatches new agents).
        // Hide it from completion everywhere else (the agent view and the welcome screen)
        dashboard_only: true,
        arg_placeholder: "path",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            CommandResult::Action(Action::DashboardOpenLocationPicker)
        } else {
            CommandResult::Action(Action::DashboardChangeLocation {
                input: trimmed.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;

    /// Build a throwaway exec ctx over the given borrows.
    /// It mirrors the inline ctx construction in `dashboard.rs`'s command tests.
    fn ctx<'a>(models: &'a ModelState, bundle: &'a BundleState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
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
    fn no_args_opens_location_picker() {
        let (models, bundle) = (ModelState::default(), BundleState::default());
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            CdCommand.run(&mut c, ""),
            CommandResult::Action(Action::DashboardOpenLocationPicker)
        ));
    }

    #[test]
    fn whitespace_only_opens_location_picker() {
        let (models, bundle) = (ModelState::default(), BundleState::default());
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            CdCommand.run(&mut c, "   "),
            CommandResult::Action(Action::DashboardOpenLocationPicker)
        ));
    }

    #[test]
    fn path_arg_changes_location() {
        let (models, bundle) = (ModelState::default(), BundleState::default());
        let mut c = ctx(&models, &bundle);
        match CdCommand.run(&mut c, "  ~/projects/foo  ") {
            CommandResult::Action(Action::DashboardChangeLocation { input }) => {
                assert_eq!(input, "~/projects/foo");
            }
            other => panic!("expected DashboardChangeLocation, got {other:?}"),
        }
    }

    #[test]
    fn metadata() {
        let cmd = CdCommand;
        assert_eq!(cmd.name(), "cd");
        assert!(cmd.takes_args());
        assert_eq!(cmd.arg_placeholder(), Some("path"));
        assert!(!cmd.description().is_empty());
        assert!(!cmd.usage().is_empty());
        // Dashboard-only hides `/cd` from completion everywhere else (the agent view, the welcome screen)
        assert!(cmd.dashboard_only(), "/cd must be dashboard-only");
    }
}
