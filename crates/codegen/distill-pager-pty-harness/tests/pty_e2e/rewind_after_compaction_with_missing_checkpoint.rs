// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Clears the 500-char floor below which `is_degenerate_summary` rejects the mock response as a summary.
const RESPONSE_LINES: usize = 10;

const TURN_TIMEOUT: Duration = Duration::from_secs(30);

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

fn prompt_index_at_compaction(path: &Path) -> u64 {
    let raw = std::fs::read(path).expect("read checkpoint file");
    let value: serde_json::Value = serde_json::from_slice(&raw).expect("checkpoint is JSON");
    value
        .get("prompt_index_at_compaction")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("no prompt_index_at_compaction in {}", path.display()))
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

/// Deleting the older of two checkpoint files (what the 30-day sweep did) must not block a rewind based on the newer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn rewind_after_compaction_with_missing_checkpoint() {
    let content = ContentController::start().await.expect("start content");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &[("GROK_COMPACTION_MODE", "summary")],
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    submit_turn(&mut harness, &content, "p0", 0);
    submit_turn(&mut harness, &content, "p1", 1);
    let checkpoints_dir = session_dir(&content, &mut harness).join("compaction_checkpoints");

    // The marker is appended after the file, so once a file exists the shell can already replay through it
    harness.inject_keys(b"/compact\r").expect("compact once");
    harness
        .wait_for_text("Compaction completed in", TURN_TIMEOUT)
        .expect("first compaction row");
    harness
        .wait_until("one checkpoint file", TURN_TIMEOUT, |_| {
            checkpoint_files(&checkpoints_dir).len() == 1
        })
        .expect("first checkpoint written");

    submit_turn(&mut harness, &content, "p2", 2);

    harness.inject_keys(b"/compact\r").expect("compact twice");
    harness
        .wait_until("two checkpoint files", TURN_TIMEOUT, |_| {
            checkpoint_files(&checkpoints_dir).len() == 2
        })
        .expect("second checkpoint written");
    harness
        .wait_for_turn_idle(Duration::from_secs(15))
        .expect("idle after second compaction");

    submit_turn(&mut harness, &content, "p3", 3);

    let checkpoints = checkpoint_files(&checkpoints_dir);
    let superseded = checkpoints
        .iter()
        .min_by_key(|path| prompt_index_at_compaction(path))
        .expect("two checkpoint files");
    std::fs::remove_file(superseded).expect("delete the superseded checkpoint");
    assert_eq!(1, checkpoint_files(&checkpoints_dir).len());

    harness.inject_keys(b"/rewind\r").expect("open picker");
    harness
        .wait_for_text("Rewind to which turn?", Duration::from_secs(15))
        .expect("rewind picker");
    harness.inject_keys(b"\r").expect("pick the newest prompt");
    // confirm_before_rewind defaults to on
    harness
        .wait_for_text("Rewind conversation to", Duration::from_secs(15))
        .expect("confirm dialog");
    harness.inject_keys(b"y").expect("confirm rewind");

    // Success restores the removed prompt into the composer
    harness
        .wait_until("rewind outcome", TURN_TIMEOUT, |h| {
            composer_holds(h, "p3") || h.contains_text("Rewind failed")
        })
        .expect("rewind settled");
    harness.update(Duration::from_millis(500));

    #[cfg(unix)]
    write_cast_if_requested(
        &harness,
        "rewind_after_compaction_with_missing_checkpoint.cast",
    );
    write_screen_dump_if_requested(&harness, "rewind_after_compaction_with_missing_checkpoint");

    let screen = harness.screen_contents();
    assert!(
        !screen.contains("Rewind failed"),
        "rewind must succeed when only a superseded checkpoint is missing\nscreen:\n{screen}"
    );
    assert!(
        composer_holds(&harness, "p3"),
        "the rewound prompt must be back in the composer\nscreen:\n{screen}"
    );
    assert!(
        !screen.contains("panicked"),
        "pager panicked\nscreen:\n{screen}"
    );

    harness.quit().expect("clean quit");
}
