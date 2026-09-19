// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;
use distill_pager_pty_harness::{InferenceEndpoint, InferenceRequestMatcher};

/// Reasoning text streamed by the mock. Must never appear in the answer text so screen assertions can tell the two apart.
const REASONING_SENTINEL: &str = "REASONINGSENTINEL";

/// GB-5502: a fullscreen → minimal → fullscreen round trip must not leave thinking folds sprung
/// open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_switch_round_trip_refolds_thinking() {
    // The model must run on the Responses backend; only the Responses API streams reasoning summary deltas (the scripted events below)
    let content = ContentController::start_with_models(vec![
        MockModel::new("test-model").with_api_backend("responses"),
    ])
    .await
    .expect("start content");
    let reasoning = format!("{REASONING_SENTINEL} pondering syllables quietly");
    let answer = format!("{MOCK_RESPONSE_SENTINEL} the answer body.");
    let _thinking_turn = content.expect_response(
        "fullscreen reasoning turn",
        InferenceRequestMatcher::foreground(InferenceEndpoint::Responses),
        ScriptedResponse::sse(sse::responses_api_reasoning_and_text_events(
            &reasoning,
            &answer,
            "test-model",
        )),
    );
    // Any further auxiliary traffic falls back to this response
    content.set_response(answer.clone());

    // Thinking blocks explicitly ON (ingestion is gated on this toggle; the sandbox `$HOME` starts with no config at all)
    std::fs::create_dir_all(content.home().join(".grok")).expect("mk .grok");
    std::fs::write(
        content.home().join(".grok/config.toml"),
        "[ui]\nshow_thinking_blocks = true\n",
    )
    .expect("write config");

    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--no-leader"],
        Some(project.path()),
    )
    .expect("spawn fullscreen pager");
    // Unanswered CPR probes abort the in-process switch to minimal.
    harness.set_respond_to_queries(true);

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit reasoning turn");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("answer rendered in fullscreen");
    harness
        .wait_for_text("Thought for", Duration::from_secs(10))
        .expect("finished thinking header in fullscreen");

    // Baseline: the finished thought auto-collapses in fullscreen, so the body must leave the screen.
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.screen_contents().contains(REASONING_SENTINEL) {
        assert!(
            Instant::now() < deadline,
            "finished thinking must collapse in fullscreen before the switch\nscreen:\n{}",
            harness.screen_contents()
        );
        harness.update(Duration::from_millis(100));
    }
    write_screen_dump_if_requested(&harness, "01-fullscreen-collapsed");

    let run_switch = |harness: &mut PtyHarness, cmd: &[u8], dropdown_row: &str| {
        inject_keys_paced(harness, cmd);
        harness
            .wait_for_text(dropdown_row, Duration::from_secs(5))
            .expect("slash dropdown row");
        harness.update(Duration::from_millis(150));
        harness.inject_keys(b"\r").expect("submit switch command");
    };

    run_switch(
        &mut harness,
        b"/minimal",
        "Switch this session to minimal (scrollback-native) mode",
    );
    harness
        .wait_for_text(MINIMAL_SWITCH_BACK_IDLE_SENTINEL, Duration::from_secs(45))
        .expect("switch to minimal");
    // Minimal's print-once contract: the reasoning body IS committed to native scrollback in full.
    harness
        .wait_for_full_text(REASONING_SENTINEL, Duration::from_secs(30))
        .expect("reasoning body committed in minimal");
    write_screen_dump_if_requested(&harness, "02-minimal-reasoning-committed");

    run_switch(
        &mut harness,
        b"/fullscreen",
        "Switch this session to fullscreen mode",
    );
    harness
        .wait_for_text("Switched to fullscreen mode", Duration::from_secs(30))
        .expect("switch back to fullscreen");

    // Require the switch toast so a terminal-restored pre-switch alt-screen
    // image can't satisfy the check before the app repaints.
    write_screen_dump_if_requested(&harness, "03-fullscreen-after-round-trip");
    write_cast_if_requested(&harness, "round-trip.cast");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        let refolded = screen.contains("Switched to fullscreen mode")
            && screen.contains("Thought for")
            && screen.contains(MOCK_RESPONSE_SENTINEL)
            && !screen.contains(REASONING_SENTINEL);
        if refolded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fullscreen must come back with the transcript visible and the thinking fold collapsed\nscreen:\n{screen}"
        );
    }
    write_screen_dump_if_requested(&harness, "03b-fullscreen-refolded");

    // The healthy frame must be stable, not a transient between repaints.
    harness.update(Duration::from_secs(1));
    let screen = harness.screen_contents();
    assert!(
        screen.contains("Thought for")
            && screen.contains(MOCK_RESPONSE_SENTINEL)
            && !screen.contains(REASONING_SENTINEL),
        "the collapsed thinking fold must persist after the round trip\nscreen:\n{screen}"
    );

    // A resize re-measures every entry height. Pre-fix, minimal's Expanded stamp
    // survived in the shared state but hid behind the stale collapsed height; the
    // re-measure is what made folds visibly spring open after the round trip.
    harness
        .resize(DEFAULT_ROWS, DEFAULT_COLS - 20)
        .expect("resize");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        if screen.contains("Thought for") && screen.contains(MOCK_RESPONSE_SENTINEL) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "transcript must survive the resize\nscreen:\n{screen}"
        );
    }
    harness.update(Duration::from_millis(500));
    write_screen_dump_if_requested(&harness, "04-fullscreen-after-resize");
    let screen = harness.screen_contents();
    assert!(
        !screen.contains(REASONING_SENTINEL),
        "thinking fold must stay collapsed through a post-round-trip resize\nscreen:\n{screen}"
    );

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked during round trip\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
