// Modified for Distill by Samuel Fajreldines, 2026.
#[allow(unused_imports)]
use super::common::*;
use distill_pager_pty_harness::{inference_request_count, inference_requests};

const REPORT: &str = "plan-overlay-report-xyz";
const APPROVE_REFUSAL: &str = "Run the slash command in the notes";

/// A complete pager command typed into the plan-approval notes box runs as a command: `/feedback <text>` POSTs to
/// the mock, the review stays parked, and neither the command nor a revision reaches the model. Approving over a
/// slash command is refused until the notes are cleared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn plan_overlay_slash_runs_command() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} first turn done."));
    let overrides = enable_feedback_posting(&content, "plan-overlay-slash");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_ops_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust", "--no-leader"],
        &overrides,
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
    std::fs::write(dir.join("plan.md"), plan_body("SLASH", 8)).expect("seed plan.md");

    let _expectation = expect_tool_turn(&content, "call_plan_slash", "exit_plan_mode", "{}".into());
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
    let turns_before = inference_request_count(&content);

    harness.inject_keys(b"s").expect("focus revise prompt");
    harness
        .wait_for_text("a:approve", Duration::from_secs(10))
        .unwrap_or_else(|e| {
            panic!(
                "revise prompt never focused after s: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    inject_keys_paced(&mut harness, format!("/feedback {REPORT}").as_bytes());
    harness.inject_keys(b"\r").expect("submit inline /feedback");

    let body = wait_for_feedback_post(&mut harness, &content, "write");
    assert!(
        body["feedbackText"]
            .as_str()
            .is_some_and(|text| text.contains(REPORT)),
        "POST body: {body}"
    );
    assert_eq!(1, content.feedback_posts().len());

    let screen = harness.screen_contents();
    assert!(
        screen.contains("request changes") && screen.contains("Waiting on plan approval"),
        "the command must leave plan approval open; screen:\n{screen}"
    );
    let full = harness.full_text();
    assert!(
        !full.contains("Plan revision sent."),
        "the command must not be sent as a revision; scrollback:\n{full}"
    );
    let leaked: Vec<String> = inference_requests(&content)
        .iter()
        .skip(turns_before)
        .filter_map(|entry| entry.body.as_ref().map(serde_json::Value::to_string))
        .filter(|body| body.contains("/feedback") || body.contains(REPORT))
        .collect();
    assert!(
        leaked.is_empty(),
        "the command must never reach the model: {leaked:?}"
    );
    let history_path = dir.join("chat_history.jsonl");
    let history = std::fs::read_to_string(&history_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", history_path.display()));
    assert!(
        history.contains("present the plan"),
        "the transcript must hold the turn that parked the review:\n{history}"
    );
    assert!(
        !history.contains("The user wants to revise the plan") && !history.contains("/feedback"),
        "the command must not land in the transcript:\n{history}"
    );

    // Approve refuses while a runnable command sits in the notes, and moves focus back to them.
    inject_keys_paced(&mut harness, b"/feedback again");
    harness.inject_keys(b"\t").expect("focus preview");
    harness.inject_keys(b"a").expect("approve over slash notes");
    harness
        .wait_for_text(APPROVE_REFUSAL, Duration::from_secs(10))
        .unwrap_or_else(|e| {
            panic!(
                "approve over a slash command must be refused: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    assert!(
        harness.contains_text("Waiting on plan approval"),
        "refused approve must leave plan approval open; screen:\n{}",
        harness.screen_contents()
    );
    assert_eq!(1, content.feedback_posts().len(), "refusal must not send");

    // The approve refusal moved focus to the notes box, so Backspace edits the command
    let backspaces = vec![0x7f_u8; "/feedback again".len()];
    inject_keys_paced(&mut harness, &backspaces);
    harness.inject_keys(b"\t").expect("focus preview");
    harness.inject_keys(b"a").expect("approve");
    harness
        .wait_until("plan approved", Duration::from_secs(20), |h| {
            h.full_text().contains("Plan approved")
        })
        .unwrap_or_else(|e| {
            panic!(
                "approve with cleared notes must close the review: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    assert!(
        !harness.full_text().contains("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
