#[allow(unused_imports)]
use super::common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn plan_revise_empty_enter_does_not_approve() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} first turn done."));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust", "--no-leader"],
        &[],
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness.inject_keys(b"go\r").expect("first turn");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(40))
        .expect("first turn streams");
    harness
        .wait_for_turn_idle(Duration::from_secs(20))
        .expect("first turn idle");

    let dir = session_dir(&content, &mut harness);
    std::fs::write(dir.join("plan.md"), plan_body("REV", 8)).expect("seed plan.md");

    let _expectation = expect_tool_turn(&content, "call_plan_rev", "exit_plan_mode", "{}".into());
    harness
        .inject_keys(b"present the plan\r")
        .expect("submit plan prompt");
    harness
        .wait_for_text("Waiting on plan approval", Duration::from_secs(60))
        .unwrap_or_else(|e| {
            panic!(
                "plan approval never parked: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    // Preview `s` is a no-op until the line-viewer key path is live. A fixed
    // sleep after first paint raced suite load: Enter on Preview opens comment
    // mode instead of toasting, so wait until the preview chrome is stable.
    harness
        .wait_until_stable(
            "plan approval preview interactive",
            Duration::from_secs(20),
            Duration::from_millis(250),
            |h| h.contains_text("request changes") && h.contains_text("Waiting on plan approval"),
        )
        .unwrap_or_else(|e| {
            panic!(
                "plan approval preview never settled: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });

    harness.inject_keys(b"s").expect("focus revise prompt");
    harness
        .wait_for_text("a:approve", Duration::from_secs(10))
        .unwrap_or_else(|e| {
            panic!(
                "revise prompt never focused after s: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });

    harness.inject_keys(b"\r").expect("empty Enter");
    harness
        .wait_for_text("Type revision notes", Duration::from_secs(10))
        .unwrap_or_else(|e| {
            panic!(
                "empty Enter must toast a nudge, not approve: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    let screen = harness.screen_contents();
    assert!(
        screen.contains("request changes") && screen.contains("Waiting on plan approval"),
        "empty Enter must leave plan approval open; screen:\n{screen}"
    );
    assert!(
        !screen.contains("Enter:approve"),
        "footer must not advertise Enter as approve; screen:\n{screen}"
    );
    assert!(
        !screen.contains("panicked"),
        "pager panicked\nscreen:\n{screen}"
    );

    harness.quit().expect("clean quit");
}
