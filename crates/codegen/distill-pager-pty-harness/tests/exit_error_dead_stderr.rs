// Modified for Distill by Samuel Fajreldines, 2026.
//! Error reports on the exit path must exit 1 when fd 2 is dead, not panic
//! (SIGABRT under `panic = "abort"`, plus a crash report on the next launch).
//!
//! Two deterministic, offline errors pin the tests to the writes in question:
//! `--memory-flush` without a prompt or session is a pre-TUI `anyhow::bail!` that lands in
//! `main()`'s generic `Error:` arm; `-p` with no credentials fails auth inside headless mode,
//! whose plain-format emitter reports it to stderr before `main()` gets the same error. The
//! headless run also carries `--include-partial-messages`, whose ignored-flag warning is
//! headless mode's other pre-auth stderr write, through the same `eprint_line` helper.

use std::process::{Command, Stdio};

use distill_pager_pty_harness::pager_binary;

/// `grok <args>` in an isolated, credential-less home.
fn grok_command(home: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(pager_binary().expect("resolve pager binary"));
    cmd.args(args)
        .env_clear()
        .env("HOME", home)
        .env("GROK_HOME", home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GROK_MANAGED_CONFIG", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    cmd
}

/// A pipe whose read end is already closed: every write fails with EPIPE.
fn dead_stderr() -> std::io::PipeWriter {
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    writer
}

/// Run once with a live stderr to confirm the invocation exits 1 and writes a line starting with
/// each of `expected_lines`, then again with a dead stderr and assert it still exits 1 instead of
/// dying by signal.
fn assert_exit_error_survives_dead_stderr(args: &[&str], expected_lines: &[&str]) {
    let home = tempfile::tempdir().unwrap();

    let control = grok_command(home.path(), args)
        .stderr(Stdio::piped())
        .output()
        .expect("spawn control run");
    let control_stderr = String::from_utf8_lossy(&control.stderr);
    assert_eq!(
        control.status.code(),
        Some(1),
        "control run {args:?} must exit 1\nstderr:\n{control_stderr}"
    );
    for expected in expected_lines {
        // Line-anchored so `main()`'s `Error: `-prefixed copy cannot stand in for the emitter's line.
        assert!(
            control_stderr.lines().any(|l| l.starts_with(expected)),
            "control run {args:?} must write a line starting with {expected:?}\nstderr:\n{control_stderr}"
        );
    }

    let status = grok_command(home.path(), args)
        .stderr(dead_stderr())
        .status()
        .expect("spawn run with dead stderr");
    assert_eq!(
        status.code(),
        Some(1),
        "{args:?} reporting to a dead stderr must still exit 1, got {status}"
    );
}

#[test]
fn exit_error_report_survives_dead_stderr() {
    assert_exit_error_survives_dead_stderr(
        &["--memory-flush"],
        &["Error: --memory-flush without a prompt"],
    );
}

#[test]
fn headless_error_report_survives_dead_stderr() {
    assert_exit_error_survives_dead_stderr(
        &[
            "-p",
            "hi",
            "--output-format",
            "plain",
            "--include-partial-messages",
        ],
        &["warning: --include-partial-messages", "Not signed in"],
    );
}
