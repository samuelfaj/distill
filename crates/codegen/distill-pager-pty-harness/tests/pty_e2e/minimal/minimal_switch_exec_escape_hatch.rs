// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// `GROK_SCREEN_MODE_SWITCH=exec` forces the legacy switch that quits, execs, and resumes, keeping that fallback covered end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_switch_exec_escape_hatch() {
    let content = ContentController::start().await.expect("start content");
    let sentinel = turn_sentinel(1);
    content.set_response(format!("{sentinel} fullscreen payload."));

    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    let binary = pager_binary().expect("resolve pager binary");
    // Answer the CPR query, or the minimal-mode probe that runs after exec silently downgrades to inline
    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--no-leader"],
        &[("GROK_SCREEN_MODE_SWITCH", "exec")],
        Some(project.path()),
    )
    .expect("spawn fullscreen pager");
    harness.set_respond_to_queries(true);

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit turn");
    harness
        .wait_for_text(&sentinel, Duration::from_secs(30))
        .expect("mock response in fullscreen");

    inject_keys_paced(&mut harness, b"/minimal");
    harness
        .wait_for_text(
            "Switch this session to minimal (scrollback-native) mode",
            Duration::from_secs(5),
        )
        .expect("slash dropdown offers /minimal");
    harness.update(Duration::from_millis(150));
    harness.inject_keys(b"\r").expect("submit /minimal");

    harness
        .wait_for_text(MINIMAL_SWITCH_BACK_IDLE_SENTINEL, Duration::from_secs(45))
        .unwrap_or_else(|e| {
            panic!(
                "exec /minimal did not reopen session in minimal mode: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    harness
        .wait_for_full_text(&sentinel, Duration::from_secs(30))
        .unwrap_or_else(|e| {
            panic!(
                "prior turn must be present after exec /minimal resume: {e}\nfull:\n{}",
                harness.full_text()
            )
        });

    // The relaunch clear must wipe the "Reopening session…" line printed before the exec
    let screen = harness.screen_contents();
    assert!(
        !screen.contains("Reopening session"),
        "main screen should be cleared on exec /minimal relaunch; leftover reopen text:\n{screen}"
    );

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked after exec /minimal\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
