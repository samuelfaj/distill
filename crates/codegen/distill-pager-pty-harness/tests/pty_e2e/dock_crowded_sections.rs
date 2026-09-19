// Modified for Distill by Samuel Fajreldines, 2026.
#[allow(unused_imports)]
use super::common::*;

/// Screen row where the dock's first section header sits. The same task
/// descriptions appear in the scrollback above, so the dock is found from the
/// bottom of the screen, never by a plain text search.
#[cfg(unix)]
fn dock_top_row(screen: &str) -> u16 {
    let lines: Vec<&str> = screen.lines().collect();
    lines
        .iter()
        .rposition(|line| line.contains("Tasks "))
        .expect("dock section header") as u16
}

/// The dock's own lines: its first section header through the row above the
/// prompt box.
#[cfg(unix)]
fn dock_lines(screen: &str) -> Vec<&str> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = dock_top_row(screen) as usize;
    let end = lines
        .iter()
        .skip(start)
        .position(|line| line.contains('╭'))
        .map_or(lines.len(), |offset| start + offset);
    let mut dock = lines[start..end].to_vec();
    while dock.last().is_some_and(|line| line.trim().is_empty()) {
        dock.pop();
    }
    dock
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_smoke Cargo test with --ignored"]
async fn a_crowded_dock_keeps_every_header_inside_its_row_cap() {
    const TASKS: usize = 12;

    let content = ContentController::start().await.expect("start content");
    content
        .server()
        .set_settings(json!({ "allow_access": true, "dock_enabled": true }));
    std::fs::write(
        content.sandbox().distill_home().join("requirements.toml"),
        "[features]\ndock = true\n",
    )
    .expect("pin dock in test requirements");

    let flag = content.home().join("dock_crowded_flag");
    let args: Vec<(String, String, String)> = (0..TASKS)
        .map(|i| {
            (
                format!("call_dock_crowded_{i}"),
                "run_terminal_command".to_string(),
                json!({
                    "command": format!(
                        "while [ ! -e {} ]; do /bin/sleep 0.2; done",
                        flag.display()
                    ),
                    "description": format!("dock task {i}"),
                    "is_background": true
                })
                .to_string(),
            )
        })
        .collect();
    let calls: Vec<(&str, &str, String)> = args
        .iter()
        .map(|(id, name, args)| (id.as_str(), name.as_str(), args.clone()))
        .collect();
    let _background = content.expect_agent_turn_with_responses(
        "dock crowded background tasks",
        ScriptedResponse::sse(responses_api_parallel_tool_call_events(&calls)),
        ScriptedResponse::sse(chat_completions_parallel_tool_call_events(&calls)),
    );
    let _settled = content.expect_agent_turn("turn settled", "DOCK_CROWDED_SETTLED");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        &[("GROK_DOCK", "1")],
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text("DOCK_CROWDED_SETTLED", Duration::from_secs(60))
        .expect("turn settled");
    harness
        .wait_for_text("show ", Duration::from_secs(20))
        .expect("show-more row");

    let screen = harness.screen_contents();
    let dock = dock_lines(&screen);
    assert!(
        dock.len() <= 8,
        "the dock never paints more than its row cap\nscreen:\n{screen}"
    );
    assert!(
        dock[0].contains(&format!("Tasks {TASKS}")),
        "the section header leads the dock\nscreen:\n{screen}"
    );
    let more_offset = dock
        .iter()
        .position(|line| line.contains("show "))
        .unwrap_or_else(|| panic!("no show-more row\nscreen:\n{screen}"));
    assert!(
        dock[more_offset].contains("more"),
        "the rows that do not fit are summarized: {:?}",
        dock[more_offset]
    );

    assert!(
        dock.iter().any(|line| line.contains("dock task 0")),
        "the section starts at its first row\nscreen:\n{screen}"
    );
    assert!(
        !dock
            .iter()
            .any(|line| line.contains(&format!("dock task {}", TASKS - 1))),
        "the last rows start out hidden\nscreen:\n{screen}"
    );

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    std::fs::write(&flag, b"done").expect("release background tasks");
    harness.quit().expect("clean quit");
}
