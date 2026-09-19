// Modified for Distill by Samuel Fajreldines, 2026.
#[allow(unused_imports)]
use super::common::*;

async fn assert_waiting_alignment(rows: u16) {
    let content = ContentController::start().await.expect("start content");
    content
        .server()
        .set_settings(json!({ "allow_access": true, "dock_enabled": true }));
    std::fs::write(
        content.sandbox().distill_home().join("requirements.toml"),
        "[features]\ndock = true\n",
    )
    .expect("pin dock in test requirements");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} done."));
    // Every SSE event, including the first, is emitted after this delay, so the turn sits in Waiting(Model) for ~3s after submit
    content.set_chunk_delay(Some(Duration::from_secs(3)));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env(
        &binary,
        rows,
        DEFAULT_COLS,
        &content,
        &[],
        &[("GROK_DOCK", "1")],
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    let welcome_screen = harness.screen_contents();
    let welcome_prompt_x = welcome_screen
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('╭'))
        .map(|line| line.len() - line.trim_start().len())
        .expect("welcome prompt border");

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");

    // Match without the trailing ellipsis so terminal width and glyph handling can't flake it
    harness
        .wait_for_text("Waiting for response", Duration::from_secs(10))
        .unwrap_or_else(|_| {
            panic!(
                "expected 'Waiting for response…' spinner before first token\nscreen:\n{}",
                harness.screen_contents()
            )
        });

    let screen = harness.screen_contents();
    let waiting = screen
        .lines()
        .find(|line| line.contains("Waiting for response"))
        .expect("waiting row");
    let prompt_border = screen
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('╭'))
        .expect("prompt border");
    let waiting_x = waiting.len() - waiting.trim_start().len();
    let prompt_x = prompt_border.len() - prompt_border.trim_start().len();
    assert_eq!(
        prompt_x, welcome_prompt_x,
        "the prompt border must not shift when home becomes a session\nwelcome:\n{welcome_screen}\nsession:\n{screen}"
    );
    assert_eq!(
        waiting_x,
        prompt_x + 1,
        "waiting spinner must sit one column inside the prompt border\nscreen:\n{screen}"
    );

    // Let the turn finish so the quit is clean and we prove the wait resolves.
    content.set_chunk_delay(None);
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("response streamed after the wait");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn waiting_for_model_label_shows_before_first_token() {
    assert_waiting_alignment(DEFAULT_ROWS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn compact_home_and_session_prompts_keep_the_same_left_edge() {
    assert_waiting_alignment(18).await;
}
