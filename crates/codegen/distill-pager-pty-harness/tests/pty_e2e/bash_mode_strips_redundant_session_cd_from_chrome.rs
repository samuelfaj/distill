// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Bash-mode strips redundant `cd $SESSION_CWD &&` from execute chrome. A `!` command with a
/// leading cd into the session cwd shows the short command in the Run header, not the long cd
/// prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[cfg(unix)]
async fn bash_mode_strips_redundant_session_cd_from_chrome() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!(
        "{MOCK_RESPONSE_SENTINEL} session ready for bash strip."
    ));

    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");
    let cwd = dunce::canonicalize(project.path()).expect("canonicalize project");
    let cwd_str = cwd.to_string_lossy();

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        Some(cwd.as_path()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Establish session so tracker has session_cwd and `!` goes through execute chrome.
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("start session");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("session ready");

    let cmd = format!("! cd {cwd_str} && printf 'STRIP_CD_OK\\n'\r");
    harness
        .inject_keys(cmd.as_bytes())
        .expect("submit bash-mode");

    harness
        .wait_for_text("STRIP_CD_OK", Duration::from_secs(30))
        .expect("command output");
    // Completed bash-mode shows "Run (user)" in execute chrome (not history `#N ! …`).
    harness
        .wait_for_text("Run (user)", Duration::from_secs(15))
        .expect("Run (user) chrome on screen");

    let screen = harness.screen_contents();
    assert!(
        !screen.contains("panicked"),
        "pager panicked\nscreen:\n{screen}"
    );
    assert!(
        screen.contains("STRIP_CD_OK"),
        "expected command output on screen:\n{screen}"
    );
    // Asserting per line avoids slicing bytes through box-drawing characters, which panics on a char boundary
    // The history line `#N ! cd …` keeps the typed command; only the Run (user) chrome loses the cd prefix
    let run_lines: Vec<&str> = screen
        .lines()
        .filter(|line| line.contains("Run (user)"))
        .collect();
    assert!(
        !run_lines.is_empty(),
        "Run (user) chrome missing on screen:\n{screen}"
    );
    assert!(
        run_lines.iter().any(|line| line.contains("printf")),
        "expected short command (printf) on Run (user) line(s) {run_lines:?}\nfull:\n{screen}"
    );
    let noisy_prefix = format!("cd {cwd_str} &&");
    assert!(
        run_lines
            .iter()
            .all(|line| !line.contains(noisy_prefix.as_str())),
        "Run chrome still shows redundant cd prefix in {run_lines:?}\nfull:\n{screen}"
    );

    harness.quit().expect("clean quit");
}
