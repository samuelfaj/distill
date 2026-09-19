// Modified for Distill by Samuel Fajreldines, 2026.
//! Safe `core.fsmonitor` override for a session-triggered `git status`.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsmonitorOverride {
    Disabled,
    BuiltIn,
}

impl FsmonitorOverride {
    pub(super) const fn git_config_arg(self) -> &'static str {
        match self {
            Self::Disabled => "core.fsmonitor=false",
            Self::BuiltIn => "core.fsmonitor=true",
        }
    }
}

/// Resolve the safe `core.fsmonitor` override for one `git status`.
/// Called from the status gather (not session spawn) and bounded by `FSMONITOR_PROBE_TIMEOUT`, so a wedged git can't stall the prompt.
pub async fn probe_fsmonitor_override(working_directory: impl Into<PathBuf>) -> FsmonitorOverride {
    let working_directory = working_directory.into();
    detect_fsmonitor_override_bounded(&mut GitProbeRunner {
        cwd: &working_directory,
    })
    .await
}

/// Bounds the probe so a stalled git/filesystem can't eat the status gather budget.
const FSMONITOR_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

async fn detect_fsmonitor_override_bounded(
    runner: &mut impl FsmonitorProbeRunner,
) -> FsmonitorOverride {
    tokio::time::timeout(FSMONITOR_PROBE_TIMEOUT, detect_fsmonitor_override(runner))
        .await
        .unwrap_or(FsmonitorOverride::Disabled)
}

/// Implementers must return `None` on spawn failure, signal, or nonzero exit (only success stdout is meaningful).
/// `detect_fsmonitor_override` treats every such `None` as unknown and falls back to disabled.
trait FsmonitorProbeRunner: Send {
    fn run_probe(
        &mut self,
        args: &[&str],
    ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send;
}

struct GitProbeRunner<'a> {
    cwd: &'a Path,
}

impl FsmonitorProbeRunner for GitProbeRunner<'_> {
    fn run_probe(
        &mut self,
        args: &[&str],
    ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
        let mut cmd = distill_tty_utils::git_command();
        cmd.args(args)
            .current_dir(self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        async move {
            let output = super::process::output_killing_group_on_drop(cmd)
                .await
                .ok()?;
            output.status.success().then_some(output.stdout)
        }
    }
}

async fn detect_fsmonitor_override(runner: &mut impl FsmonitorProbeRunner) -> FsmonitorOverride {
    // Read the raw effective value first
    // A `--type=bool` query converts every matching value, so a shadowed helper *pathname* would make a repo-local `true` fail to convert
    // The raw read tells us which case we're in
    let Some(config) = runner
        .run_probe(&["config", "--null", "--get", "core.fsmonitor"])
        .await
    else {
        return FsmonitorOverride::Disabled;
    };
    let Some(config) = config.strip_suffix(b"\0") else {
        return FsmonitorOverride::Disabled;
    };
    if config.contains(&0) {
        return FsmonitorOverride::Disabled;
    }
    let Ok(config) = str::from_utf8(config) else {
        return FsmonitorOverride::Disabled;
    };

    let configured = if ["true", "yes", "on"]
        .iter()
        .any(|value| config.eq_ignore_ascii_case(value))
    {
        true
    } else if ["false", "no", "off"]
        .iter()
        .any(|value| config.eq_ignore_ascii_case(value))
    {
        false
    } else {
        // Uncommon spelling: ask git to convert it, `--fixed-value`-filtered to the raw string
        // The filter stops a shadowed helper pathname from hijacking the conversion
        let typed_args = [
            "config",
            "--null",
            "--type=bool",
            "--fixed-value",
            "--get",
            "core.fsmonitor",
            config,
        ];
        matches!(
            runner.run_probe(&typed_args).await.as_deref(),
            Some(b"true\0")
        )
    };
    if !configured {
        return FsmonitorOverride::Disabled;
    }

    // Keep the built-in daemon only when git advertises it
    // Git 2.35.1 and older read `true` as a hook *pathname* and would run a program named `true`; versions before 2.26 can hide tracked changes
    // The feature line is the capability signal
    let Some(build_options) = runner.run_probe(&["version", "--build-options"]).await else {
        return FsmonitorOverride::Disabled;
    };
    if build_options
        .split(|byte| *byte == b'\n')
        .any(|line| line.trim_ascii() == b"feature: fsmonitor--daemon")
    {
        FsmonitorOverride::BuiltIn
    } else {
        FsmonitorOverride::Disabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CannedProbes(std::collections::VecDeque<(Vec<&'static str>, Option<Vec<u8>>)>);

    impl CannedProbes {
        fn new(responses: Vec<(Vec<&'static str>, Option<Vec<u8>>)>) -> Self {
            Self(responses.into())
        }
    }

    impl FsmonitorProbeRunner for CannedProbes {
        fn run_probe(
            &mut self,
            args: &[&str],
        ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
            let (expected, response) = self.0.pop_front().expect("unexpected extra probe");
            assert_eq!(args, expected.as_slice(), "unexpected probe arguments");
            async move { response }
        }
    }

    fn config_args() -> Vec<&'static str> {
        vec!["config", "--null", "--get", "core.fsmonitor"]
    }

    fn typed_config_args(value: &'static str) -> Vec<&'static str> {
        vec![
            "config",
            "--null",
            "--type=bool",
            "--fixed-value",
            "--get",
            "core.fsmonitor",
            value,
        ]
    }

    fn capability_args() -> Vec<&'static str> {
        vec!["version", "--build-options"]
    }

    struct StalledProbes;

    impl FsmonitorProbeRunner for StalledProbes {
        fn run_probe(
            &mut self,
            _args: &[&str],
        ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
            std::future::pending()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fsmonitor_probe_stall_is_bounded_and_falls_back_to_disabled() {
        let start = tokio::time::Instant::now();
        assert_eq!(
            detect_fsmonitor_override_bounded(&mut StalledProbes).await,
            FsmonitorOverride::Disabled
        );
        assert_eq!(start.elapsed(), FSMONITOR_PROBE_TIMEOUT);
    }

    const DAEMON_BUILD_OPTIONS: &[u8] = b"git version 2.46.0\nfeature: fsmonitor--daemon\n";

    #[tokio::test]
    async fn fsmonitor_unset_config_is_disabled() {
        let mut runner = CannedProbes::new(vec![(config_args(), None)]);
        assert_eq!(
            detect_fsmonitor_override(&mut runner).await,
            FsmonitorOverride::Disabled
        );
    }

    #[tokio::test]
    async fn fsmonitor_helper_pathname_is_disabled() {
        let mut runner = CannedProbes::new(vec![
            (config_args(), Some(b"/usr/local/bin/watchman\0".to_vec())),
            (typed_config_args("/usr/local/bin/watchman"), None),
        ]);
        assert_eq!(
            detect_fsmonitor_override(&mut runner).await,
            FsmonitorOverride::Disabled
        );
    }

    #[tokio::test]
    async fn fsmonitor_boolean_true_with_daemon_support_is_builtin() {
        let mut runner = CannedProbes::new(vec![
            (config_args(), Some(b"true\0".to_vec())),
            (capability_args(), Some(DAEMON_BUILD_OPTIONS.to_vec())),
        ]);
        assert_eq!(
            detect_fsmonitor_override(&mut runner).await,
            FsmonitorOverride::BuiltIn
        );
    }

    #[tokio::test]
    async fn fsmonitor_boolean_true_without_daemon_support_is_disabled() {
        let mut runner = CannedProbes::new(vec![
            (config_args(), Some(b"true\0".to_vec())),
            (capability_args(), Some(b"git version 2.35.1\n".to_vec())),
        ]);
        assert_eq!(
            detect_fsmonitor_override(&mut runner).await,
            FsmonitorOverride::Disabled
        );
    }

    #[tokio::test]
    async fn fsmonitor_nonzero_integer_normalizes_through_git() {
        let mut runner = CannedProbes::new(vec![
            (config_args(), Some(b"2\0".to_vec())),
            (typed_config_args("2"), Some(b"true\0".to_vec())),
            (capability_args(), Some(DAEMON_BUILD_OPTIONS.to_vec())),
        ]);
        assert_eq!(
            detect_fsmonitor_override(&mut runner).await,
            FsmonitorOverride::BuiltIn
        );
    }
}
