// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::app::{Command, PagerArgs};
use clap::Parser;
use pretty_assertions::assert_eq;
fn agent_args(mode: &[&str]) -> Box<AgentArgs> {
    let parsed =
        PagerArgs::try_parse_from(["grok", "agent"].into_iter().chain(mode.iter().copied()))
            .expect("parse agent arguments");
    let Some(Command::Agent(args)) = parsed.command else {
        panic!("agent subcommand required");
    };
    args
}
#[test]
fn shell_runtime_preserves_leader_resolution() {
    let config = Config::default();
    let raw_config = toml::toml! {
        [cli] use_leader = true
    }
    .into();
    for (arguments, eligible) in [
        (vec!["stdio"], true),
        (vec!["--leader", "stdio"], true),
        (vec!["--no-leader", "stdio"], true),
        (vec!["headless"], true),
        (vec!["serve"], false),
        (vec!["leader"], false),
        (vec![], true),
    ] {
        let args = agent_args(&arguments);
        let runtime = Backend::Shell.resolve(&args, &config, &raw_config);
        let expected = resolve_leader_mode(
            args.leader,
            args.no_leader,
            &raw_config,
            config.remote_settings.as_ref(),
            eligible,
            distill_sandbox::requested_confinement_profile(),
        );
        assert_eq!(
            (Backend::Shell, expected),
            (runtime.backend, runtime.leader_mode()),
            "arguments: {arguments:?}"
        );
    }
}
