// Modified for Distill by Samuel Fajreldines, 2026.
//! PTY child spawn: the Unix fork/exec session spawn (setsid, controlling
//! TTY, pdeathsig) and child-environment assembly; Windows keeps
//! portable-pty's spawn and shares the environment hygiene via [`EnvSink`].

use std::ffi::OsStr;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use anyhow::{Context, Result};
use portable_pty::CommandBuilder;
#[cfg(any(unix, test))]
use distill_test_support::TestSandbox;

use crate::pty::EnvOp;

const CLIPBOARD_SINK_ENV_VARS: &[&str] = &["GROK_OSC52_SINK", "LC_GROK_OSC52_SINK"];

/// Host / wrap appearance hints that would make `theme=auto` non-deterministic
/// in PTY tests (layout depends on the resolved palette).
const APPEARANCE_ENV_VARS: &[&str] = &[
    "GROK_APPEARANCE",
    "LC_GROK_APPEARANCE",
    "GROK_THEME",
    "LC_GROK_THEME",
    "COLORFGBG",
];

/// Markers the pager's brand/mux/editor detection reads. A leak reclassifies the child (tmux, Cursor before TERM_PROGRAM, nvim OSC 52).
/// Keep in sync with that detection source.
const HOST_TERMINAL_ENV_VARS: &[&str] = &[
    // Brand chain (detect_terminal_brand_from_env), in detection order.
    "CURSOR_TRACE_ID",
    "VSCODE_GIT_ASKPASS_MAIN",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERMINAL_EMULATOR",
    "WEZTERM_VERSION",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERM_SESSION_ID",
    "KITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "TERMINATOR_UUID",
    "VTE_VERSION",
    "WT_SESSION",
    // Multiplexer / Byobu markers (detect_multiplexer_from_env,
    // detect_byobu_from_env, detect_tmux_meta_from_env).
    "TMUX",
    "TMUX_PANE",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "STY",
    "BYOBU_BACKEND",
    "BYOBU_CONFIG_DIR",
    "BYOBU_DISTRO",
    "CMUX_SOCKET_PATH",
    "CMUX_PANEL_ID",
    "CMUX_BUNDLE_ID",
    "HERDR_ENV",
    // Embedded editor markers (embedded_editor_from_env).
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "VIM_TERMINAL",
    "INSIDE_EMACS",
];

/// Child-environment destination for [`apply_child_env`]: Windows'
/// `CommandBuilder` keeps its registry-merged base environment, Unix
/// assembles a plain map — one hygiene list, two containers.
pub(crate) trait EnvSink {
    fn set_var(&mut self, key: &OsStr, value: &OsStr);
    fn remove_var(&mut self, key: &OsStr);
}

impl EnvSink for CommandBuilder {
    fn set_var(&mut self, key: &OsStr, value: &OsStr) {
        self.env(key, value);
    }

    fn remove_var(&mut self, key: &OsStr) {
        self.env_remove(key);
    }
}

#[cfg(unix)]
impl EnvSink for std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    fn set_var(&mut self, key: &OsStr, value: &OsStr) {
        self.insert(key.to_owned(), value.to_owned());
    }

    fn remove_var(&mut self, key: &OsStr) {
        self.remove(key);
    }
}

/// Sandbox baseline or inherited parent env, then the hygiene pass and caller overrides from [`apply_child_env`].
#[cfg(unix)]
pub(crate) fn compute_child_env(
    sandbox: Option<&TestSandbox>,
    env: &[EnvOp<'_>],
) -> std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    let mut map: std::collections::BTreeMap<_, _> = match sandbox {
        Some(sandbox) => sandbox.env().into_iter().collect(),
        None => std::env::vars_os().collect(),
    };
    // portable-pty always seeded SHELL. Pin `/bin/sh` when the host lacks it instead of a passwd lookup.
    map.entry(std::ffi::OsString::from("SHELL"))
        .or_insert_with(|| std::ffi::OsString::from("/bin/sh"));
    apply_child_env(&mut map, env);
    map
}

/// Explicit cwd if it is a directory, else the child's HOME. Unlike portable-pty, a bad HOME falls back to the parent cwd instead of failing the spawn.
#[cfg(unix)]
pub(crate) fn resolve_child_cwd(
    cwd: Option<&Path>,
    child_home: Option<&std::ffi::OsString>,
) -> Result<std::path::PathBuf> {
    if let Some(dir) = cwd
        && dir.is_dir()
    {
        return Ok(dir.to_path_buf());
    }
    if let Some(home) = child_home {
        let home = std::path::PathBuf::from(home);
        if home.is_dir() {
            return Ok(home);
        }
    }
    // The harness always provides a sandbox HOME or inherits the host's, so
    // this fallback is effectively unreachable.
    std::env::current_dir().context("failed to resolve a working directory for the PTY child")
}

/// Hygiene then caller overrides last. Callers seed the sink first (sandbox baseline or inherited parent env).
pub(crate) fn apply_child_env<S: EnvSink>(cmd: &mut S, env: &[EnvOp<'_>]) {
    // Set TERM so the pager renders with full color support.
    cmd.set_var(OsStr::new("TERM"), OsStr::new("xterm-256color"));
    // Leaked NO_COLOR renders colorless and makes style asserts untestable. Tests may re-set these after this pass.
    for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
        cmd.remove_var(OsStr::new(color_var));
    }
    // Leaked SSH vars make the detector report SSH and disable the drag-drop image classifier.
    for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
        cmd.remove_var(OsStr::new(ssh_var));
    }
    // A harness launched under `grok wrap` must not silently confirm clipboard
    // delivery for no-sink scenarios. Explicit sink tests re-inject a marker
    // through `env` after this hygiene pass.
    for sink_var in CLIPBOARD_SINK_ENV_VARS {
        cmd.remove_var(OsStr::new(sink_var));
    }
    for appearance_var in APPEARANCE_ENV_VARS {
        cmd.remove_var(OsStr::new(appearance_var));
    }
    // Host TERM_PROGRAM/mux/editor markers would make the child adopt that terminal's quirks.
    for term_var in HOST_TERMINAL_ENV_VARS {
        cmd.remove_var(OsStr::new(term_var));
    }
    for operation in env {
        match operation {
            EnvOp::Set(key, value) => cmd.set_var(key, value),
            EnvOp::Remove(key) => cmd.remove_var(key),
        }
    }
}

/// Own fork/exec because portable-pty has no pre_exec. Linux PDEATHSIG(SIGKILL) reaps the child if the spawner dies without Drop.
/// The kernel signals the spawning *thread*. Drop the harness before that thread exits, or an idle `spawn_blocking` worker SIGKILLs the pager mid-test.
#[cfg(unix)]
pub(crate) fn spawn_pty_session_child(
    mut cmd: std::process::Command,
    master: &dyn portable_pty::MasterPty,
) -> Result<std::process::Child> {
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::process::CommandExt as _;

    let tty = master
        .tty_name()
        .context("PTY master has no slave tty name")?;
    let tty = std::ffi::CString::new(tty.into_os_string().into_vec())
        .context("PTY slave tty name contains a NUL byte")?;

    // First post-fork action, so the unprotected window is minimal. SIGKILL reaps a child that cannot service TERM.
    // Survives exec only for non-setuid fixtures; the kernel clears PDEATHSIG across a privileged exec.
    #[cfg(target_os = "linux")]
    distill_tty_utils::kill_on_parent_death_std_with(&mut cmd, libc::SIGKILL);

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the hook only performs fork-safe syscalls plus the same /dev/fd
    // sweep portable-pty itself runs between fork and exec.
    unsafe {
        cmd.pre_exec(move || pty_session_pre_exec(&tty));
    }
    // The child is enrolled post-spawn in a TestProcessTree (group kill on
    // teardown) and pdeathsig-guarded on Linux, which is exactly the enrollment
    // this lint asks for; ProcessScope is a production-pager concept.
    #[allow(clippy::disallowed_methods)]
    cmd.spawn().context("failed to spawn PTY child")
}

/// Child-side session/tty setup between fork and exec for
/// [`spawn_pty_session_child`], parity with portable-pty's own pre_exec
/// (which this spawn path replaces).
#[cfg(unix)]
fn pty_session_pre_exec(tty: &std::ffi::CStr) -> io::Result<()> {
    // SAFETY: fork-to-exec child; only async-signal-safe libc, errno wrapped without allocation.
    // The /dev/fd sweep matches portable-pty's own pre_exec.
    unsafe {
        // Clear inherited signal dispositions and mask…
        for signo in [
            libc::SIGCHLD,
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGALRM,
        ] {
            libc::signal(signo, libc::SIG_DFL);
        }
        let mut empty_set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty_set);
        libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

        // Session leader so a group kill takes descendants. Raw setsid: the detach helper's setpgid fallback would break TIOCSCTTY.
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
        }

        // …attach the PTY slave as the controlling terminal on stdio (required
        // for SIGWINCH delivery on resize)…
        let fd = libc::open(tty.as_ptr(), libc::O_RDWR);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // TIOCSCTTY's constant type is platform-dependent, hence `as _`.
        #[allow(clippy::cast_lossless)]
        if libc::ioctl(fd, libc::TIOCSCTTY as _, 0) == -1 {
            return Err(io::Error::last_os_error());
        }
        for stdio_fd in 0..=2 {
            if libc::dup2(fd, stdio_fd) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        if fd > 2 {
            libc::close(fd);
        }

        // Also closes std's exec-status pipe, so a bad binary is a successful spawn plus instant exit — parity with portable-pty.
        portable_pty::unix::close_random_fds();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    /// Seed into the sink, not process-global `set_var` (racy under parallel tests). This covers the Windows CommandBuilder sink.
    #[test]
    fn apply_child_env_strips_all_host_terminal_markers() {
        let mut cmd = CommandBuilder::new("true");
        for var in HOST_TERMINAL_ENV_VARS {
            cmd.env(var, "polluted");
        }
        for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
            cmd.env(ssh_var, "polluted");
        }
        for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
            cmd.env(color_var, "polluted");
        }
        for sink_var in CLIPBOARD_SINK_ENV_VARS {
            cmd.env(sink_var, "polluted");
        }
        for appearance_var in APPEARANCE_ENV_VARS {
            cmd.env(appearance_var, "polluted");
        }
        // Sandboxed launches remove unrelated inherited variables before
        // re-applying the baseline and explicit overrides.
        cmd.env("GROK_SCROLL_LOG", "/tmp/scroll.jsonl");
        let sandbox = TestSandbox::new();
        cmd.env_clear();
        sandbox.apply_to_command_builder(&mut cmd);

        apply_child_env(&mut cmd, &[]);

        for var in HOST_TERMINAL_ENV_VARS {
            assert!(
                cmd.get_env(var).is_none(),
                "host terminal marker {var} leaked into the child env"
            );
        }
        for ssh_var in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_AUTH_SOCK"] {
            assert!(
                cmd.get_env(ssh_var).is_none(),
                "SSH marker {ssh_var} leaked into the child env"
            );
        }
        for color_var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
            assert!(
                cmd.get_env(color_var).is_none(),
                "color override {color_var} leaked into the child env"
            );
        }
        for sink_var in CLIPBOARD_SINK_ENV_VARS {
            assert!(
                cmd.get_env(sink_var).is_none(),
                "clipboard sink marker {sink_var} leaked into the child env"
            );
        }
        for appearance_var in APPEARANCE_ENV_VARS {
            assert!(
                cmd.get_env(appearance_var).is_none(),
                "appearance hint {appearance_var} leaked into the child env"
            );
        }
        assert_eq!(
            cmd.get_env("TERM").and_then(|v| v.to_str()),
            Some("xterm-256color")
        );
        assert_eq!(
            cmd.get_env("GROK_SCROLL_LOG").and_then(|v| v.to_str()),
            None,
            "hermetic baseline must remove unrelated inherited vars"
        );
        assert_eq!(
            cmd.get_env("GROK_HOME").and_then(|v| v.to_str()),
            sandbox.distill_home().to_str()
        );
    }

    #[test]
    fn apply_child_env_uses_sandbox_baseline() {
        let sandbox = TestSandbox::new();
        let mut cmd = CommandBuilder::new("true");
        cmd.env_clear();
        sandbox.apply_to_command_builder(&mut cmd);

        apply_child_env(&mut cmd, &[]);

        assert_eq!(
            cmd.get_env("HOME").and_then(|v| v.to_str()),
            sandbox.home().to_str()
        );
        assert_eq!(
            cmd.get_env("GROK_HOME").and_then(|v| v.to_str()),
            sandbox.distill_home().to_str()
        );
        assert_eq!(cmd.get_env("GROK_LEADER_SOCKET"), None);
    }

    /// Sandbox baseline wins, hygiene strips markers seeded into the same map, caller ops apply last.
    #[cfg(unix)]
    #[test]
    fn unix_child_env_strips_markers_and_keeps_sandbox_baseline() {
        let sandbox = TestSandbox::new();
        let mut env = compute_child_env(Some(&sandbox), &[EnvOp::set("PTY_TEST_EXTRA", "1")]);
        assert_eq!(
            env.get(OsStr::new("HOME")).and_then(|v| v.to_str()),
            sandbox.home().to_str()
        );
        assert_eq!(
            env.get(OsStr::new("TERM")).and_then(|v| v.to_str()),
            Some("xterm-256color")
        );
        assert_eq!(
            env.get(OsStr::new("PTY_TEST_EXTRA"))
                .and_then(|v| v.to_str()),
            Some("1")
        );

        // Inherited-mode pollution lands in the same map the hygiene pass
        // edits; seed markers directly instead of process-global set_var.
        for var in HOST_TERMINAL_ENV_VARS {
            env.insert(OsString::from(var), OsString::from("polluted"));
        }
        apply_child_env(&mut env, &[]);
        for var in HOST_TERMINAL_ENV_VARS {
            assert!(
                !env.contains_key(OsStr::new(*var)),
                "host terminal marker {var} leaked into the unix child env"
            );
        }
    }

    /// Portable-pty parity: explicit cwd wins only when it's a directory;
    /// otherwise the child's HOME.
    #[cfg(unix)]
    #[test]
    fn unix_child_cwd_falls_back_to_child_home() {
        let sandbox = TestSandbox::new();
        let home = OsString::from(sandbox.home().as_os_str());

        let explicit = resolve_child_cwd(Some(sandbox.workspace()), Some(&home)).unwrap();
        assert_eq!(explicit, sandbox.workspace());

        let missing = sandbox.root().join("does-not-exist");
        let fallback = resolve_child_cwd(Some(&missing), Some(&home)).unwrap();
        assert_eq!(fallback, sandbox.home());

        let none = resolve_child_cwd(None, Some(&home)).unwrap();
        assert_eq!(none, sandbox.home());
    }

    #[test]
    fn apply_child_env_remove_deletes_sandbox_credential() {
        let sandbox = TestSandbox::builder()
            .mock_url("http://127.0.0.1:43123/v1")
            .build();
        let mut cmd = CommandBuilder::new("true");
        cmd.env_clear();
        sandbox.apply_to_command_builder(&mut cmd);

        apply_child_env(&mut cmd, &[EnvOp::remove("XAI_API_KEY")]);

        assert_eq!(cmd.get_env("XAI_API_KEY"), None);
        assert_eq!(
            cmd.get_env("GROK_XAI_API_BASE_URL")
                .and_then(|v| v.to_str()),
            Some("http://127.0.0.1:43123/v1")
        );
    }

    #[test]
    fn inherited_env_projection_preserves_unrelated_ambient_vars() {
        let mut cmd = CommandBuilder::new("true");
        cmd.env("AMBIENT_MARKER", "inherited");
        apply_child_env(&mut cmd, &[EnvOp::set("EXPLICIT_MARKER", "set")]);

        assert_eq!(
            cmd.get_env("AMBIENT_MARKER")
                .and_then(|value| value.to_str()),
            Some("inherited")
        );
        assert_eq!(
            cmd.get_env("EXPLICIT_MARKER")
                .and_then(|value| value.to_str()),
            Some("set")
        );
    }

    #[test]
    fn apply_child_env_caller_env_overrides_survive_strips() {
        let mut cmd = CommandBuilder::new("true");
        cmd.env("TMUX", "/tmp/host-tmux,999,0");
        cmd.env("CURSOR_TRACE_ID", "host-cursor");

        apply_child_env(
            &mut cmd,
            &[
                EnvOp::set("TERM_PROGRAM", "vscode"),
                EnvOp::set("NVIM", "/tmp/fake-nvim.sock"),
                EnvOp::set("TERM", "xterm-kitty"),
                EnvOp::set("GROK_OSC52_SINK", "1"),
            ],
        );

        // Host pollution is gone…
        assert!(cmd.get_env("TMUX").is_none());
        assert!(cmd.get_env("CURSOR_TRACE_ID").is_none());
        // …but caller-injected markers (applied after strips) survive,
        // including overriding the harness's own TERM default.
        assert_eq!(
            cmd.get_env("TERM_PROGRAM").and_then(|v| v.to_str()),
            Some("vscode")
        );
        assert_eq!(
            cmd.get_env("NVIM").and_then(|v| v.to_str()),
            Some("/tmp/fake-nvim.sock")
        );
        assert_eq!(
            cmd.get_env("TERM").and_then(|v| v.to_str()),
            Some("xterm-kitty")
        );
        assert_eq!(
            cmd.get_env("GROK_OSC52_SINK").and_then(|v| v.to_str()),
            Some("1"),
            "explicit sink scenarios must be able to re-inject the marker"
        );
    }
}
