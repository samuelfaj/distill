// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// A bare program name that PATH cannot resolve (the alias case, and the old hard-ENOENT failure)
/// must be handed to `$SHELL`.
#[test]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
#[cfg(unix)]
fn wrap_not_found_alias_routes_via_shell_contract() {
    let (_dir, shell) = fake_argv_echo_shell();
    let (code, raw) = run_wrap(
        &["grok-wrap-e2e-alias-xx", "with space"],
        &[("SHELL", &shell)],
    );

    assert!(
        raw.contains("ARG:-i"),
        "shell must be invoked interactively (rc files / aliases)\nraw:\n{raw}"
    );
    assert!(raw.contains("ARG:-c"), "shell must get -c\nraw:\n{raw}");
    assert!(
        raw.contains("ARG:grok-wrap-e2e-alias-xx 'with space'"),
        "rejoined line must keep the first word bare and quote the tail\nraw:\n{raw}"
    );
    assert_eq!(
        code,
        Some(0),
        "fake shell exits 0; wrap must propagate it\nraw:\n{raw}"
    );
}
