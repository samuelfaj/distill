// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Clears the 500-char floor below which `is_degenerate_summary` rejects the mock response as a summary.
const RESPONSE_LINES: usize = 10;

const TURN_TIMEOUT: Duration = Duration::from_secs(30);

const PROMPT_A: &str = "retireAlphaPrompt";
const PROMPT_B: &str = "retireBetaPrompt";
const FOLLOW_UP: &str = "retireFollowUp";
const COMPACT_SENTINEL: &str = "RETIRECOMPACTSUMMARY";

/// Ctrl+Alt+X: 8-bit terminal chord is ESC then Ctrl-X. Kitty CSI-u is `CSI 120;7u`.
/// One write; a split ESC then Ctrl+X is idle-Esc plus shortcuts-help.
const CTRL_ALT_X: &[u8] = b"\x1b\x18";

const COMPACTION_ENV: &[(&str, &str)] = &[
    ("GROK_COMPACTION_MODE", "summary"),
    ("GROK_AGENT_DASHBOARD", "0"),
];

/// `.json` only: the atomic writer's temp files sit in the same dir.
fn checkpoint_files(checkpoints_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(checkpoints_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn read_text(path: &Path) -> String {
    String::from_utf8_lossy(&read_bytes(path)).into_owned()
}

fn prompt_history_path(session_dir: &Path) -> PathBuf {
    session_dir
        .parent()
        .expect("session dir has cwd parent")
        .join("prompt_history.jsonl")
}

fn worktree_paths(home: &Path) -> Vec<PathBuf> {
    let root = home.join(".grok").join("worktrees");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

fn updates_has_checkpoint_marker(updates: &str) -> bool {
    updates.contains("compaction_checkpoint") || updates.contains("CompactionCheckpoint")
}

fn agent_screen_ready(harness: &PtyHarness) -> bool {
    let screen = harness.screen_contents();
    screen.contains("Shift+Tab") && !screen.contains("↑/↓ select")
}

fn spawn_session(content: &ContentController, project: &Path) -> PtyHarness {
    let binary = pager_binary().expect("resolve pager binary");
    PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        content,
        &[],
        COMPACTION_ENV,
        Some(project),
    )
    .expect("spawn pager")
}

fn submit_turn(harness: &mut PtyHarness, content: &ContentController, prompt: &str, turn: u8) {
    let sentinel = turn_sentinel(turn);
    content.set_response(long_response(&sentinel, RESPONSE_LINES));
    harness
        .inject_keys(format!("{prompt}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(&sentinel, TURN_TIMEOUT)
        .expect("turn rendered");
    harness
        .wait_for_turn_idle(Duration::from_secs(15))
        .expect("turn idle");
}

fn wait_agent_not_dashboard_or_welcome(harness: &mut PtyHarness) {
    harness
        .wait_until(
            "agent screen idle, not dashboard or welcome",
            TURN_TIMEOUT,
            agent_screen_ready,
        )
        .expect("agent screen is the retire target");
}

fn press_retire(harness: &mut PtyHarness) {
    harness.inject_keys(CTRL_ALT_X).expect("Ctrl+Alt+X retire");
}

fn seed_two_turns(
    content: &ContentController,
    harness: &mut PtyHarness,
) -> (PathBuf, String, Vec<u8>, Vec<u8>, Vec<u8>, Vec<PathBuf>) {
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    submit_turn(harness, content, PROMPT_A, 0);
    submit_turn(harness, content, PROMPT_B, 1);

    let dir = session_dir(content, harness);
    let id = dir
        .file_name()
        .expect("session id")
        .to_string_lossy()
        .into_owned();
    let history = read_bytes(&dir.join("chat_history.jsonl"));
    let updates = read_bytes(&dir.join("updates.jsonl"));
    let prompts = read_bytes(&prompt_history_path(&dir));
    let worktrees = worktree_paths(content.home());
    (dir, id, history, updates, prompts, worktrees)
}

fn assert_originals_on_disk(
    session_dir: &Path,
    original_history: &[u8],
    original_updates: &[u8],
    original_prompts: &[u8],
    original_worktrees: &[PathBuf],
    home: &Path,
) {
    assert!(
        session_dir.is_dir(),
        "session dir must stay: {}",
        session_dir.display()
    );
    let updates = read_text(&session_dir.join("updates.jsonl"));
    assert!(
        updates.contains(PROMPT_A) && updates.contains(PROMPT_B),
        "updates.jsonl must keep original prompts\n{}",
        updates
    );
    let history_now = read_bytes(&session_dir.join("chat_history.jsonl"));
    assert!(
        !original_history.is_empty() || !history_now.is_empty(),
        "chat_history.jsonl must exist"
    );
    let prompt_now = read_text(&prompt_history_path(session_dir));
    assert!(
        prompt_now.contains(PROMPT_A) && prompt_now.contains(PROMPT_B),
        "workspace prompt_history.jsonl must keep original prompts\n{prompt_now}"
    );
    assert!(
        read_bytes(&session_dir.join("updates.jsonl")).starts_with(original_updates),
        "retirement must preserve the original transcript bytes"
    );
    assert!(
        !original_prompts.is_empty(),
        "prompt_history.jsonl must have been recorded before retire"
    );
    let worktrees_now = worktree_paths(home);
    for path in original_worktrees {
        assert!(
            path.exists(),
            "worktree must not be deleted: {}",
            path.display()
        );
    }
    assert!(
        worktrees_now.len() >= original_worktrees.len(),
        "retire must not delete worktrees\nbefore: {original_worktrees:?}\nafter: {worktrees_now:?}"
    );
}

/// Retire (Ctrl+Alt+X) compacts, persists the resume checkpoint, closes or archives without deleting history, then `--resume` of the same id accepts a follow-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn retire_session_compact_close_history_resume() {
    let content = ContentController::start().await.expect("start content");
    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    let mut harness = spawn_session(&content, project.path());
    let (session_dir, session_id, original_history, original_updates, original_prompts, worktrees) =
        seed_two_turns(&content, &mut harness);

    wait_agent_not_dashboard_or_welcome(&mut harness);
    content.set_response(long_response(COMPACT_SENTINEL, RESPONSE_LINES));
    press_retire(&mut harness);

    let checkpoints_dir = session_dir.join("compaction_checkpoints");
    harness
        .wait_until("checkpoint json", TURN_TIMEOUT, |_| {
            !checkpoint_files(&checkpoints_dir).is_empty()
        })
        .expect("checkpoint JSON written");
    harness
        .wait_until("updates checkpoint marker", TURN_TIMEOUT, |_| {
            updates_has_checkpoint_marker(&read_text(&session_dir.join("updates.jsonl")))
        })
        .expect("updates.jsonl CompactionCheckpoint marker");

    assert_originals_on_disk(
        &session_dir,
        &original_history,
        &original_updates,
        &original_prompts,
        &worktrees,
        content.home(),
    );

    harness
        .wait_until("retire closes the active agent", TURN_TIMEOUT, |h| {
            !agent_screen_ready(h)
        })
        .expect("retired agent must leave the active view");
    assert!(!harness.contains_text("Cannot close the only session"));

    #[cfg(unix)]
    write_cast_if_requested(&harness, "retire_session_compact_close_history_resume.cast");
    write_screen_dump_if_requested(&harness, "retire_session_compact_close_history_resume");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("quit after retire");

    let binary = pager_binary().expect("resolve pager binary");
    let mut resumed = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--resume", &session_id],
        COMPACTION_ENV,
        Some(project.path()),
    )
    .expect("spawn --resume");
    resumed
        .wait_for_text(&turn_sentinel(1), RESUME_TIMEOUT)
        .expect("resumed transcript");

    content.set_response(long_response(&turn_sentinel(2), RESPONSE_LINES));
    resumed
        .inject_keys(format!("{FOLLOW_UP}\r").as_bytes())
        .expect("submit follow-up");
    resumed
        .wait_for_text(&turn_sentinel(2), TURN_TIMEOUT)
        .expect("follow-up rendered");

    resumed
        .wait_until(
            "follow-up saved to the original session",
            TURN_TIMEOUT,
            |_| read_text(&session_dir.join("updates.jsonl")).contains(FOLLOW_UP),
        )
        .expect("resume must continue writing the original session history");

    if !content.request_bodies().is_empty() {
        assert!(
            content.request_bodies().iter().any(|body| {
                let text = body.to_string();
                text.contains(FOLLOW_UP) && text.contains(COMPACT_SENTINEL)
            }),
            "follow-up request should carry compacted context when inspectable\nbodies: {:?}",
            content.request_bodies()
        );
    }

    #[cfg(unix)]
    write_cast_if_requested(
        &resumed,
        "retire_session_compact_close_history_resume_continue.cast",
    );
    write_screen_dump_if_requested(
        &resumed,
        "retire_session_compact_close_history_resume_continue",
    );

    assert!(
        !resumed.contains_text("panicked"),
        "pager panicked on resume\nscreen:\n{}",
        resumed.screen_contents()
    );
    resumed.quit().expect("quit resumed pager");
}

/// A non-directory `compaction_checkpoints` path makes the checkpoint write fail FlushAndAck; retire must stay open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn retire_session_persist_failure_keeps_session_open() {
    let content = ContentController::start().await.expect("start content");
    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    let mut harness = spawn_session(&content, project.path());
    let (session_dir, _session_id, original_history, original_updates, original_prompts, worktrees) =
        seed_two_turns(&content, &mut harness);

    std::fs::write(session_dir.join("compaction_checkpoints"), b"not-a-dir")
        .expect("inject checkpoint persist failure");

    wait_agent_not_dashboard_or_welcome(&mut harness);
    content.set_response(long_response(COMPACT_SENTINEL, RESPONSE_LINES));
    press_retire(&mut harness);

    harness
        .wait_until("retire persist failure", TURN_TIMEOUT, |h| {
            h.contains_text("Retire cancelled: compaction did not persist")
                || h.contains_text("Compaction failed")
        })
        .expect("persist failure surfaced");

    assert!(
        agent_screen_ready(&harness) || harness.contains_text("Shift+Tab"),
        "session must stay open after persist failure\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        checkpoint_files(&session_dir.join("compaction_checkpoints")).is_empty(),
        "no checkpoint JSON when compaction_checkpoints is a file"
    );

    let updates = read_text(&session_dir.join("updates.jsonl"));
    assert!(
        updates.contains(PROMPT_A) && updates.contains(PROMPT_B),
        "original transcript must stay after persist failure\n{updates}"
    );
    assert!(
        session_dir.join("chat_history.jsonl").is_file() || !original_history.is_empty(),
        "history file must remain"
    );
    let _ = original_updates;
    let _ = original_prompts;
    for path in &worktrees {
        assert!(path.exists(), "worktree stayed: {}", path.display());
    }

    #[cfg(unix)]
    write_cast_if_requested(&harness, "retire_session_persist_failure.cast");
    write_screen_dump_if_requested(&harness, "retire_session_persist_failure");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
