// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Signal-death e2e: SIGTERM delivered to `grok wrap` itself (an external kill, or the HUP a
/// closing terminal sends) must not skip cleanup. Drop handlers never run on signal death, so wrap
/// needs an explicit signal path.
#[test]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
#[cfg(unix)]
fn wrap_sigterm_restores_terminal_and_exit_code() {
    let (code, raw) = run_wrap_driving(
        &[
            "/bin/sh",
            "-c",
            r"printf '\033[?1003h\033[?1006h\033[?2004h\033[?25l'; printf 'WRAP_E2E_READY'; sleep 60",
        ],
        &[],
        |harness| {
            harness
                .wait_until(
                    "child mode enables to flow through wrap",
                    WRAP_TIMEOUT,
                    |h| String::from_utf8_lossy(h.raw_output()).contains("WRAP_E2E_READY"),
                )
                .expect("child never latched its modes");
            harness.send_signal(libc::SIGTERM).expect("SIGTERM wrap");
        },
    );

    assert_eq!(
        code,
        Some(143),
        "SIGTERM death must surface as the conventional 128+15\nraw:\n{raw:?}"
    );
    for needle in ["\x1b[?1003l", "\x1b[?1006l", "\x1b[?2004l", "\x1b[?25h"] {
        assert!(
            raw.contains(needle),
            "wrap's signal path must emit {needle:?} for the latched mode\nraw:\n{raw:?}"
        );
    }
}
