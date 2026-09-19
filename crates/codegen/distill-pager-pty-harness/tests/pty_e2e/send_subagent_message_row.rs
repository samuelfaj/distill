// Modified for Distill by Samuel Fajreldines, 2026.
//! PTY: the `send_subagent_message` rows of a parent transcript against a live background child.
//! The parent spawns "sleeper", whose first model call is a flag-gated foreground command, then steers, queues,
//! interjects, and sends to a bogus id. Asserted: each collapsed row is the bare header (no text, no reason, no raw
//! id); expanding the steer row shows ` · steer` and the full text; expanding the rejected row shows the shell's
//! reason and the raw `Subagent ID:`.
// Unix only: the child's hold is a `/bin/sleep` loop.
#![cfg(unix)]
#[allow(unused_imports)]
use super::common::*;
use distill_test_support::{Conversation, MockToolCall, Tool};

/// Spawn rejects an injected id that is not a UUIDv7; the bogus one shares its shape and ends in a readable tail.
const CHILD_ID: &str = "01a06b44-0000-7000-8000-00000000c41d";
const BOGUS_ID: &str = "01a06b44-0000-7000-8000-0000deadbeef";
pub(crate) const CHILD_LABEL: &str = "General \u{201c}sleeper\u{201d}";
const STEER_TEXT: &str = "STEER_MARKER re-check the arm64 job";
const QUEUE_TEXT: &str = "QUEUE_MARKER then summarize";
const INTERJECT_TEXT: &str = "INTERJECT_MARKER stop after this step";
const BOGUS_TEXT: &str = "BOGUS_MARKER hello";
pub(crate) const PARENT_DONE: &str = "PARENT_TURN_DONE";
const WAKE_DONE: &str = "PARENT_WAKE_DONE";
/// The shell's reason for an unknown id, verbatim.
const REJECTED_REASON: &str = "Subagent not found or not owned by this session.";
/// `keys` has no Left; the collapse chord mirrors `keys::RIGHT`.
const LEFT: &[u8] = b"\x1b[D";
/// Scripted turns run with no model; the budget covers a loaded CI host, not the flow itself.
pub(crate) const ROW_TIMEOUT: Duration = Duration::from_secs(60);

/// The parent and child scripts plus the spawned pager, driven to the point where every send row has settled.
/// The child keeps polling `hold_flag` until the caller writes it, so the sends land on an active child.
pub(crate) struct SendMessageScenario {
    content: ContentController,
    pub(crate) harness: PtyHarness,
    hold_flag: PathBuf,
}

impl SendMessageScenario {
    pub(crate) async fn start() -> Self {
        let content = ContentController::start().await.expect("start content");
        let hold_flag = content.home().join("sleeper_hold_flag");
        let send = |text: &str, delivery: Option<&str>| {
            let mut args = json!({ "subagent_id": CHILD_ID, "text": text });
            if let Some(delivery) = delivery {
                args["delivery"] = json!(delivery);
            }
            MockToolCall::new(Tool::SendMessage, args)
        };
        // Conversations are keyed by session id, so the child's first call is conversation 2 whatever the arrival
        // order. The wake the child's completion triggers is the parent's second, call-free turn.
        content.server().set_conversations(vec![
            Conversation::nth(1)
                .calls([
                    MockToolCall::new(
                        Tool::Task,
                        json!({
                            "description": "sleeper",
                            "prompt": "SLEEPER_CHILD_PROMPT wait for the flag",
                            "subagent_type": "general-purpose",
                            "background": true,
                            "task_id": CHILD_ID,
                        }),
                    ),
                    send(STEER_TEXT, None),
                    send(QUEUE_TEXT, Some("queue")),
                    send(INTERJECT_TEXT, Some("interject")),
                    MockToolCall::new(
                        Tool::SendMessage,
                        json!({ "subagent_id": BOGUS_ID, "text": BOGUS_TEXT }),
                    ),
                ])
                .reply(PARENT_DONE)
                .reply(WAKE_DONE),
            Conversation::nth(2)
                .calls([MockToolCall::new(
                    Tool::Shell,
                    json!({
                        "command": format!(
                            "while [ ! -e {} ]; do /bin/sleep 0.2; done",
                            hold_flag.display()
                        ),
                        "description": "hold until released",
                    }),
                )])
                .reply("CHILD_DONE"),
        ]);

        let binary = pager_binary().expect("resolve pager binary");
        // --yolo auto-approves the spawn, the sends, and the child's command; the tool is gated off by default.
        let mut harness = PtyHarness::spawn_with_content_env_ops_in_dir(
            &binary,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            &content,
            &["--yolo", "--trust"],
            &[EnvOp::set("GROK_ACTIVE_AGENT_MESSAGES", "1")],
            Some(content.home()),
        )
        .expect("spawn pager");
        harness
            .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
            .expect("welcome");
        harness
            .inject_keys(format!("{PROMPT}\r").as_bytes())
            .expect("submit prompt");

        let mut scenario = SendMessageScenario {
            content,
            harness,
            hold_flag,
        };
        for row in [
            format!("Message sent to {CHILD_LABEL}"),
            format!("Message queued for {CHILD_LABEL}"),
            format!("Message interjected to {CHILD_LABEL}"),
            "Message rejected \u{b7} subagent \u{2026}deadbeef".to_owned(),
            PARENT_DONE.to_owned(),
        ] {
            scenario.wait_for_row(&row);
        }
        scenario
    }

    /// The screen line carrying `header` ends with it: a collapsed row is the header alone.
    fn assert_row_is_bare(&self, header: &str) {
        let screen = self.harness.screen_contents();
        let line = screen
            .lines()
            .find(|line| line.contains(header))
            .unwrap_or_else(|| panic!("{header:?} not on screen:\n{screen}"));
        assert!(
            line.trim_end().ends_with(header),
            "collapsed row must carry no tail: {line:?}\nscreen:\n{screen}"
        );
    }

    pub(crate) fn wait_for_row(&mut self, text: &str) {
        self.harness
            .wait_for_text(text, ROW_TIMEOUT)
            .unwrap_or_else(|error| {
                panic!(
                    "{error}\n--- non-system messages ---\n{}",
                    dump_non_system_messages(&self.content.request_bodies())
                )
            });
    }

    /// Tab hands the keys to the scrollback; the footer's `Space:prompt` hint confirms it owns them.
    pub(crate) fn focus_scrollback(&mut self) {
        self.harness.inject_keys(b"\t").expect("focus scrollback");
        self.wait_for_row("Space:prompt");
    }

    /// A click selects the row carrying `text`; the `›` caret on that row confirms the selection.
    pub(crate) fn select_row(&mut self, text: &str) {
        let screen = self.harness.screen_contents();
        let (row, col) = locate_screen_text(&screen, text)
            .unwrap_or_else(|| panic!("{text:?} not on screen:\n{screen}"));
        let click = format!(
            "{}{}",
            sgr_mouse(0, row, col, 'M'),
            sgr_mouse(0, row, col, 'm')
        );
        self.harness
            .inject_keys(click.as_bytes())
            .expect("click row");
        self.wait_for_row(&format!("\u{203a} {text}"));
    }

    /// Release the child, let its completion wake the parent, and quit; the mock's drop check then proves
    /// both scripts ran through.
    pub(crate) fn finish(mut self, cast_name: &str) {
        std::fs::write(&self.hold_flag, b"done").expect("release the child");
        self.wait_for_row(WAKE_DONE);
        self.harness
            .wait_for_turn_idle(ROW_TIMEOUT)
            .expect("wake turn idle");
        assert!(
            !self.harness.contains_text("panicked"),
            "pager panicked\nscreen:\n{}",
            self.harness.screen_contents()
        );
        write_cast_if_requested(&self.harness, cast_name);
        self.harness.inject_keys(b"\x11").expect("ctrl-q once");
        self.harness.update(Duration::from_millis(200));
        self.harness.inject_keys(b"\x11").expect("ctrl-q confirm");
        self.harness.quit().expect("clean quit");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn send_subagent_message_row() {
    let mut scenario = SendMessageScenario::start().await;
    write_screen_dump_if_requested(&scenario.harness, "send_row_collapsed_120");
    let steer_header = format!("Message sent to {CHILD_LABEL}");
    for header in [
        steer_header.clone(),
        format!("Message queued for {CHILD_LABEL}"),
        format!("Message interjected to {CHILD_LABEL}"),
        "Message rejected \u{b7} subagent \u{2026}deadbeef".to_owned(),
    ] {
        scenario.assert_row_is_bare(&header);
    }
    let screen = scenario.harness.screen_contents();
    for absent in [
        STEER_TEXT,
        QUEUE_TEXT,
        INTERJECT_TEXT,
        BOGUS_TEXT,
        REJECTED_REASON,
        CHILD_ID,
        BOGUS_ID,
        "Sent message to subagent",
    ] {
        assert!(
            !screen.contains(absent),
            "{absent:?} must not render while collapsed\nscreen:\n{screen}"
        );
    }
    // The `sending to` row is not asserted: admission is in-memory, so the Pending and Completed updates usually
    // land in one pager frame; that grammar is pinned by the block's unit tests instead.

    // Right expands the steer row: the delivery suffix, then the full text without captions.
    scenario.focus_scrollback();
    scenario.select_row(&steer_header);
    scenario
        .harness
        .inject_keys(keys::RIGHT)
        .expect("expand row");
    scenario.wait_for_row(&format!("Message sent to {CHILD_LABEL} \u{b7} steer"));
    scenario.wait_for_row(STEER_TEXT);
    write_screen_dump_if_requested(&scenario.harness, "send_row_expanded_steer");
    let screen = scenario.harness.screen_contents();
    assert!(
        !screen.contains("To:") && !screen.contains("Message:"),
        "expanded body carries no captions\nscreen:\n{screen}"
    );

    // The rejected row expands to the shell's verbatim reason and the raw id no spawn named.
    scenario.select_row("Message rejected \u{b7} subagent");
    scenario
        .harness
        .inject_keys(keys::RIGHT)
        .expect("expand rejected row");
    scenario.wait_for_row(REJECTED_REASON);
    scenario.wait_for_row(&format!("Subagent ID: {BOGUS_ID}"));
    write_screen_dump_if_requested(&scenario.harness, "send_row_expanded_rejected");
    scenario
        .harness
        .inject_keys(LEFT)
        .expect("collapse rejected row");
    scenario
        .harness
        .wait_for_text_absent(REJECTED_REASON, ROW_TIMEOUT)
        .expect("Left folds the reason away");

    scenario.finish("send_subagent_message_row.cast");
}
