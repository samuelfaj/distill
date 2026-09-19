// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// `$HOME` is itself a git repo (dotfiles in home) and the session launches in a subdir (`<home>/proj`) with its own repo-local `.mcp.json`.
/// The trust question must render for the subdir and the accepted grant must persist keyed on the subdir, never on `$HOME`.
/// The reported bug resolved both up to `$HOME` because the git up-walk landed on the home repo root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn folder_trust_home_git_repo_subdir_keys_on_subdir() {
    let content = ContentController::start().await.expect("start content");

    // $HOME is a dotfiles-style git repo; the launch dir is a subdir with its own repo-local `.mcp.json`, so the subdir has something to gate
    git2::Repository::init(content.home()).expect("git init $HOME");
    let proj = content.home().join("proj");
    std::fs::create_dir_all(&proj).expect("create proj subdir");
    std::fs::write(proj.join(".mcp.json"), "{}").expect("write proj/.mcp.json");

    let env_refs = trust_env(true);
    let cwd = proj.to_str().expect("utf8 proj path");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--cwd", cwd],
        &env_refs,
    )
    .expect("spawn pager");

    // Check the trust store directly, not via `folder_is_trusted`: the assertions below query both the
    // subdir and `$HOME` against the same store file.
    let store_path = content.home().join(".grok").join(TRUST_FILE_NAME);

    // The question renders (keyed on the subdir), and the store is empty first.
    harness
        .wait_for_text(TRUST_QUESTION_SENTINEL, WELCOME_TIMEOUT)
        .expect("trust question renders for the subdir");
    assert!(
        !store_trusts(&store_path, &proj),
        "store must be empty before the user answers",
    );

    // Accepting persists a grant that trusts the subdir. The child writes the store asynchronously after `y`, so reload it each poll iteration.
    harness.inject_keys(b"y").expect("inject y");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut trusted = false;
    while Instant::now() < deadline {
        if store_trusts(&store_path, &proj) {
            trusted = true;
            break;
        }
        harness.update(Duration::from_millis(100));
    }
    assert!(
        trusted,
        "accepting must persist a grant that trusts the subdir\nscreen:\n{}",
        harness.screen_contents()
    );

    // Core regression: the grant must NOT trust $HOME (the reported bug keyed on $HOME).
    assert!(
        !store_trusts(&store_path, content.home()),
        "trust must key on the subdir, never on $HOME",
    );

    harness.quit().expect("clean quit");
}
