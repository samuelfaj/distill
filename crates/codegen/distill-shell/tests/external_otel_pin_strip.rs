// Modified for Distill by Samuel Fajreldines, 2026.
//! Dedicated binary: production `remove_var` must not race the lib test suite.

use distill_shell::agent::external_otel_pin;
use distill_tty_utils::{detach_std_command, pager_env};
use std::process::Stdio;

const DECOY: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const DECOY_VALUE: &str = "http://127.0.0.1:9";
const PROBE_ENV: &str = "GROK_OTEL_PIN_STRIP_PROBE";

#[test]
fn production_strip_removes_process_env_and_child_does_not_inherit_decoy() {
    if std::env::var(PROBE_ENV).as_deref() == Ok("1") {
        std::process::exit(if std::env::var(DECOY).is_ok() { 1 } else { 0 });
    }

    // SAFETY: this binary has a single test; nothing else reads process env.
    unsafe { std::env::set_var(DECOY, DECOY_VALUE) };
    assert_eq!(std::env::var(DECOY).as_deref(), Ok(DECOY_VALUE));

    let req: toml::Value = toml::from_str(
        r#"
            [telemetry]
            otel_endpoint = "http://127.0.0.1:4318"
            "#,
    )
    .unwrap();
    // SAFETY: this binary has a single test; nothing else reads process env.
    let stripped = unsafe { external_otel_pin::apply_process_env_strip(&req) };
    assert!(
        stripped.iter().any(|n| n == DECOY),
        "strip list must include the decoy: {stripped:?}"
    );
    assert!(
        std::env::var(DECOY).is_err(),
        "production strip must remove_var the decoy"
    );
    assert!(
        !std::env::vars().any(|(k, _)| k == DECOY),
        "decoy must be gone from the process env map"
    );

    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "--exact",
        "production_strip_removes_process_env_and_child_does_not_inherit_decoy",
    ]);
    cmd.env(PROBE_ENV, "1");
    cmd.stdin(Stdio::null());
    cmd.envs(pager_env());
    detach_std_command(&mut cmd);
    let status = cmd.status().expect("probe child");
    assert!(
        status.success(),
        "child must not inherit the decoy endpoint (exit={status})"
    );
}
