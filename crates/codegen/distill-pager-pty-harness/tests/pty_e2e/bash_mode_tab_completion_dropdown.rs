// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Two seeded files sharing the typed prefix (equal length, so ranking ties break on name and `AAA` is deterministically the top row).
/// Both reach the screen only through the Tab-opened dropdown.
const FILE_AAA: &str = "SUGGESTAAA.txt";
const FILE_BBB: &str = "SUGGESTBBB.txt";
/// A seeded shell-history line sharing the same typed prefix.
/// Tab fetches are token-only: this row must NEVER appear on a Tab (a mixed set would break the terminal-style Tab completion of the file rows).
const HISTORY_LINE: &str = "cat SUGGESTHISTROW-42";
const HISTORY_SENTINEL: &str = "SUGGESTHISTROW";
/// What the mocked user types: `!` flips into bash mode (consumed by the prompt), then the shared prefix of both files.
/// That prefix is exactly their LCP, so the first Tab opens the dropdown instead of prefix-filling.
const TYPED_PREFIX: &str = "!cat SUGGEST";

/// Env: NO suggestion flag; Tab completion in bash mode is always on, and this test is the
/// acceptance proof. The PTY child inherits the parent env, so a dev shell exporting the flag must
/// not turn it on here.
fn suggestions_env(histfile: &Path) -> Vec<(String, String)> {
    vec![
        ("SHELL".into(), "/bin/bash".into()),
        ("GROK_SUGGESTIONS".into(), "0".into()),
        ("HISTFILE".into(), histfile.to_string_lossy().into_owned()),
    ]
}

fn seed_history(content: &ContentController) -> std::path::PathBuf {
    let histfile = content.home().join(".bash_history");
    std::fs::write(&histfile, format!("{HISTORY_LINE}\n")).expect("seed shell history");
    histfile
}

/// Bash-mode completion acceptance with NO env flag: Tab fetches token-only candidates, opens the
/// dropdown, and accepts the selection in place. The prefix-matching history line stays out (Tab
/// fetches run only the token providers).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[cfg(unix)]
async fn bash_mode_tab_accepts_dropdown_item_in_place() {
    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");
    std::fs::write(project.path().join(FILE_AAA), "").expect("seed AAA file");
    std::fs::write(project.path().join(FILE_BBB), "").expect("seed BBB file");
    let cwd = dunce::canonicalize(project.path()).expect("canonicalize project");

    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} session up."));
    let histfile = seed_history(&content);

    let env = suggestions_env(&histfile);
    let env_refs: Vec<(&str, &str)> = env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &env_refs,
        Some(&cwd),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Establish a session: bash mode lives on the agent-view prompt (the welcome prompt never completes)
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("start session");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("session ready");

    harness
        .inject_keys(TYPED_PREFIX.as_bytes())
        .expect("type bash prefix");
    // No as-you-type pipeline: neither candidate may render before Tab.
    let screen = harness.screen_contents();
    assert!(
        !screen.contains(FILE_AAA) && !screen.contains(FILE_BBB),
        "nothing may complete before Tab without the env flag:\n{screen}"
    );

    harness.inject_keys(b"\t").expect("press Tab");
    // The fetch Tab started lands and opens the dropdown with both files
    harness
        .wait_for_text(FILE_AAA, Duration::from_secs(15))
        .expect("dropdown lists the top file candidate");
    harness
        .wait_for_text(FILE_BBB, Duration::from_secs(5))
        .expect("dropdown lists the second file candidate");
    let screen = harness.screen_contents();
    assert!(
        !screen.contains(HISTORY_SENTINEL),
        "token-only Tab fetch must not surface history rows:\n{screen}"
    );

    harness.inject_keys(b"\x1b[B").expect("press Down");
    harness.inject_keys(b"\t").expect("press Tab to accept");
    // A trailing char proves the accepted token is real prompt text (the dropdown never showed this combined string)
    harness.inject_keys(b"Y").expect("type trailing char");
    harness
        .wait_for_text(&format!("cat {FILE_BBB}Y"), Duration::from_secs(10))
        .expect("selected item accepted into the prompt in place");

    let screen = harness.screen_contents();
    assert!(
        !screen.contains("panicked"),
        "pager panicked\nscreen:\n{screen}"
    );
    assert!(
        !screen.contains(FILE_AAA),
        "the other candidate must vanish with the closed dropdown:\n{screen}"
    );

    harness.quit().expect("clean quit");
}
