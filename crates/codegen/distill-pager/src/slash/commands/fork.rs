//! `/fork`: branch the current session into a peer top-level agent.
//!
//! The command parses optional flags (`--worktree`, `--no-worktree`) and an optional free-form directive.
//! It returns [`Action::Fork`](crate::app::actions::Action::Fork) carrying a [`ForkArgs`] payload.
//! The placeholder construction, modal routing, and effect emission live in `dispatch::dispatch_fork`.
//! The fork itself is dispatched in `dispatch_fork_resolved`, after the worktree question is resolved and the placeholder spawn succeeds.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// [`parse_fork_args`] returns this, and [`Action::Fork`](crate::app::actions::Action::Fork) carries it to the dispatcher.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ForkArgs {
    /// `None` -> open the worktree question modal (the user is asked every time. the choice is never persisted).
    pub worktree_override: Option<bool>,
    /// Optional first prompt for the new session. Whitespace-trimmed.
    /// `None` when the user typed `/fork` (with or without flags) and no directive text; the new agent then opens with no first prompt.
    pub directive: Option<String>,
}

/// Parse the raw argument string after `/fork`. The args are user-typed text, and a directive that happens to begin
/// with `--` must not be rejected. `--worktree` and `--no-worktree` cannot both appear.
pub fn parse_fork_args(args: &str) -> Result<ForkArgs, String> {
    let mut worktree_override: Option<bool> = None;
    let mut rest = args.trim_start();

    while !rest.is_empty() {
        let (flag, after) = match rest.split_once(char::is_whitespace) {
            Some(parts) => parts,
            None => (rest, ""),
        };
        match flag {
            "--worktree" => {
                if worktree_override == Some(false) {
                    return Err("--worktree and --no-worktree are mutually exclusive".into());
                }
                if worktree_override == Some(true) {
                    return Err("--worktree specified twice".into());
                }
                worktree_override = Some(true);
                rest = after.trim_start();
            }
            "--no-worktree" => {
                if worktree_override == Some(true) {
                    return Err("--worktree and --no-worktree are mutually exclusive".into());
                }
                if worktree_override == Some(false) {
                    return Err("--no-worktree specified twice".into());
                }
                worktree_override = Some(false);
                rest = after.trim_start();
            }
            "--at" => {
                return Err("--at is not supported in this version".into());
            }
            _ => break,
        }
    }

    let directive = if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    };
    Ok(ForkArgs {
        worktree_override,
        directive,
    })
}

pub struct ForkCommand;

impl SlashCommand for ForkCommand {
    slash_meta! {
        name: "fork",
        description: "Branch the current session into a peer agent",
        usage: "/fork [--worktree|--no-worktree] [directive]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[directive]",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match parse_fork_args(args) {
            Ok(parsed) => CommandResult::Action(Action::Fork(parsed)),
            Err(msg) => CommandResult::Error(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    // -- parse_fork_args ---------------------------------------------------

    #[test]
    fn parse_empty_returns_none_directive_and_no_override() {
        let parsed = parse_fork_args("").expect("empty args parse");
        assert_eq!(parsed.worktree_override, None);
        assert_eq!(parsed.directive, None);
    }

    #[test]
    fn parse_directive_only_returns_directive_with_no_override() {
        let parsed =
            parse_fork_args("explore the rate-limit hypothesis").expect("directive-only parse");
        assert_eq!(parsed.worktree_override, None);
        assert_eq!(
            parsed.directive.as_deref(),
            Some("explore the rate-limit hypothesis")
        );
    }

    #[test]
    fn parse_worktree_flag_alone_sets_override_true() {
        let parsed = parse_fork_args("--worktree").expect("--worktree alone parse");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive, None);
    }

    #[test]
    fn parse_no_worktree_flag_alone_sets_override_false() {
        let parsed = parse_fork_args("--no-worktree").expect("--no-worktree alone parse");
        assert_eq!(parsed.worktree_override, Some(false));
        assert_eq!(parsed.directive, None);
    }

    #[test]
    fn parse_worktree_flag_with_directive_sets_both() {
        let parsed = parse_fork_args("--worktree investigate the bug")
            .expect("--worktree + directive parse");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive.as_deref(), Some("investigate the bug"));
    }

    #[test]
    fn parse_no_worktree_flag_with_directive_sets_both() {
        let parsed =
            parse_fork_args("--no-worktree quick fix").expect("--no-worktree + directive parse");
        assert_eq!(parsed.worktree_override, Some(false));
        assert_eq!(parsed.directive.as_deref(), Some("quick fix"));
    }

    #[test]
    fn parse_worktree_then_no_worktree_is_mutual_exclusion_error() {
        let err = parse_fork_args("--worktree --no-worktree x")
            .expect_err("conflicting flags must error");
        assert!(
            err.contains("mutually exclusive"),
            "error should explain mutual exclusion: {err}"
        );
    }

    #[test]
    fn parse_no_worktree_then_worktree_is_mutual_exclusion_error() {
        let err = parse_fork_args("--no-worktree --worktree x")
            .expect_err("conflicting flags must error");
        assert!(
            err.contains("mutually exclusive"),
            "error should explain mutual exclusion: {err}"
        );
    }

    #[test]
    fn parse_worktree_repeated_returns_error() {
        let err = parse_fork_args("--worktree --worktree foo")
            .expect_err("duplicate --worktree must error");
        assert!(
            err.contains("twice"),
            "error should mention duplicate: {err}"
        );
    }

    #[test]
    fn parse_at_flag_returns_friendly_v1_error() {
        let err = parse_fork_args("--at 3 directive").expect_err("--at must error in v1");
        assert!(
            err.contains("--at is not supported"),
            "error should mention --at deferral: {err}"
        );
    }

    #[test]
    fn parse_leading_whitespace_is_trimmed_before_flag_lookup() {
        let parsed = parse_fork_args("   --worktree foo bar").expect("leading whitespace allowed");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive.as_deref(), Some("foo bar"));
    }

    #[test]
    fn parse_unknown_token_is_treated_as_directive_start() {
        // Conservative behaviour: a bareword that isn't a recognised flag becomes the directive
        // `/fork --foo bar` is not rejected as a typo; the model receives `--foo bar` as its first prompt
        let parsed = parse_fork_args("--foo bar").expect("unknown flag parse");
        assert_eq!(parsed.worktree_override, None);
        assert_eq!(parsed.directive.as_deref(), Some("--foo bar"));
    }

    #[test]
    fn parse_extra_whitespace_between_flag_and_directive_is_trimmed() {
        let parsed =
            parse_fork_args("--worktree    investigate").expect("extra whitespace allowed");
        assert_eq!(parsed.worktree_override, Some(true));
        assert_eq!(parsed.directive.as_deref(), Some("investigate"));
    }

    // -- ForkCommand SlashCommand impl ------------------------------------

    fn make_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        let bundle = Box::leak(Box::new(crate::app::bundle::BundleState::default()));
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
    fn run_no_args_returns_fork_action_with_default_args() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "") {
            CommandResult::Action(Action::Fork(args)) => {
                assert_eq!(args.worktree_override, None);
                assert_eq!(args.directive, None);
            }
            other => panic!("expected Action(Fork(..)), got {other:?}"),
        }
    }

    #[test]
    fn run_worktree_with_directive_returns_action_carrying_both() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "--worktree fix the test") {
            CommandResult::Action(Action::Fork(args)) => {
                assert_eq!(args.worktree_override, Some(true));
                assert_eq!(args.directive.as_deref(), Some("fix the test"));
            }
            other => panic!("expected Action(Fork(..)), got {other:?}"),
        }
    }

    #[test]
    fn run_conflicting_flags_returns_error_result() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "--worktree --no-worktree") {
            CommandResult::Error(msg) => {
                assert!(msg.contains("mutually exclusive"), "got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_at_flag_returns_error_result() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ForkCommand;
        match cmd.run(&mut ctx, "--at 5") {
            CommandResult::Error(msg) => {
                assert!(msg.contains("--at is not supported"), "got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn metadata_matches_design() {
        let cmd = ForkCommand;
        assert_eq!(cmd.name(), "fork");
        assert!(cmd.takes_args(), "/fork accepts args");
        assert!(!cmd.args_required(), "/fork allows no args");
        assert_eq!(cmd.arg_placeholder(), Some("[directive]"));
    }
}
