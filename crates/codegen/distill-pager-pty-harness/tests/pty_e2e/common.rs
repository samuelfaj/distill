// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared constants and helpers for PTY e2e tests.
//!
//! Individual test modules import via `use super::common::*`.

pub(crate) use serde_json::json;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::time::{Duration, Instant};
pub(crate) use distill_pager_pty_harness::{
    AgentTurnExpectation, ContentController, EnvOp, MockModel, PtyExitPoll, PtyHarness,
    ScriptedResponse, SseEvent, keys, oauth_credential_ops, pager_binary, seed_fake_oauth, sse,
    wait_for_labels_absent, wait_for_model_via_new_sessions,
};

/// Default PTY size used by every e2e test.
/// Large enough to render the welcome screen without wrapping, small enough to make `screen_contents()` scans cheap.
pub(crate) const DEFAULT_ROWS: u16 = 50;

pub(crate) const DEFAULT_COLS: u16 = 120;

/// Default wait-for-welcome timeout.
/// The pager spawns a child shell agent, which can take a few seconds on cold build directories.
pub(crate) const WELCOME_TIMEOUT: Duration = Duration::from_secs(20);

/// Wait budget for a `--continue` / resume to replay the prior transcript back into scrollback.
/// Match [`WRAP_TIMEOUT`] (120s) for the same contention reason, not because resume is slow when
/// run alone.
pub(crate) const RESUME_TIMEOUT: Duration = Duration::from_secs(120);

/// Substring we wait for on the welcome screen.
/// Matches the menu label `"Quit"` (`render_welcome_done` / gate menus).
/// Case-sensitive, so it does **not** match the lowercase `"quit"` hint line during `AuthState::Authenticating`.
pub(crate) const WELCOME_SCREEN_SENTINEL: &str = "Quit";

/// Prompt sent to the agent in content-driven tests.
/// Short so it submits quickly and doesn't wrap.
pub(crate) const PROMPT: &str = "go";

/// Response the mock server will stream back.
/// Must contain a stable, unambiguous sentinel word that we can `wait_for_text` on.
pub(crate) const MOCK_RESPONSE_SENTINEL: &str = "MOCKRESPONSE";

/// The sandbox's unified log (shell-written; forwarded pager entries land here too).
/// `"logs"` mirrors `distill_telemetry::unified_log::LOG_DIR`; the harness does not link telemetry.
pub(crate) fn unified_log_path(content: &ContentController) -> PathBuf {
    content
        .sandbox()
        .distill_home()
        .join("logs")
        .join("unified.jsonl")
}

// ── Undo-tip e2e helpers ────────────────────────────────────────────────

/// Suffix of the undo-tip banner, now "Input cleared · ctrl+z to undo" on all platforms (terminals don't forward Cmd+Z to a raw-mode TUI).
/// Asserting only the suffix keeps the check independent of which chord the banner names.
pub(crate) const UNDO_TIP_SENTINEL: &str = "to undo";

pub(crate) const SEND_NOW_TIP_SENTINEL: &str = "to send now";

/// A draft of FIRE_PEAK_LEN (20) or more chars.
/// The first keystroke (or Ctrl+N) leaves Welcome; the rest lands in the agent composer.
pub(crate) const SUBSTANTIAL_DRAFT: &[u8] = b"aaaaaaaaaaaaaaaaaaaaaaaaa";

/// Leave the optimistic home screen via Ctrl+N so the rest of the test runs
/// in the agent composer (stash, undo tip, image chips, F2 settings).
pub(crate) fn leave_home(harness: &mut PtyHarness) {
    harness
        .inject_keys(keys::CTRL_N)
        .expect("Ctrl+N leave home");
    // Agent chrome; "New worktree" can linger in other copy.
    harness
        .wait_for_text("Shift+Tab", Duration::from_secs(20))
        .expect("left the welcome home screen");
}

/// Typing and the kill are injected separately (with a settle in between) to avoid racing the async
/// welcome-to-session promotion.
pub(crate) fn wipe_substantial_draft(harness: &mut PtyHarness) {
    leave_home(harness);
    harness
        .inject_keys(SUBSTANTIAL_DRAFT)
        .expect("type substantial draft");
    harness
        .wait_for_text("aaaaaaaaaa", Duration::from_secs(10))
        .expect("draft renders in the agent prompt before the wipe");
    harness.inject_keys(b"\x15").expect("Ctrl+U kill-to-BOL");
}

/// Contextual-hints opt-in. The feature ships default-OFF.
pub(crate) const CONTEXTUAL_HINTS_ENV: &[(&str, &str)] = &[("GROK_CONTEXTUAL_HINTS", "1")];

/// Collect short OSC 8 payloads for assertion failure messages.
pub(crate) fn osc8_snippets(raw: &str) -> String {
    let mut out = Vec::new();
    for part in raw.split("\x1b]8;") {
        if part.is_empty() {
            continue;
        }
        let end = part
            .find(['\u{7}', '\x1b'])
            .unwrap_or_else(|| part.len().min(120));
        let snippet = &part[..end];
        if snippet.contains("file://") || snippet.contains("Demo") {
            out.push(snippet.chars().take(160).collect::<String>());
        }
        if out.len() >= 6 {
            break;
        }
    }
    if out.is_empty() {
        "(no file:// OSC 8 payloads found)".into()
    } else {
        out.join(" | ")
    }
}

pub(crate) fn long_response(sentinel: &str, lines: usize) -> String {
    let mut s = String::with_capacity(lines * 64);
    s.push_str(sentinel);
    s.push_str(" — scroll payload follows.\n\n");
    for i in 0..lines {
        s.push_str(&format!(
            "Line {i}: the quick brown fox jumps over the lazy dog and keeps on going.\n"
        ));
    }
    s
}

/// A response that renders to at least `rows` terminal rows. Each `line N` becomes exactly one
/// rendered row. A 60-line prose paragraph reflows to only ~30 rows at typical widths and fits on
/// screen, so it would not overflow into scrollback.
pub(crate) fn tall_response(sentinel: &str, rows: usize) -> String {
    let mut s = String::with_capacity(rows * 24);
    s.push_str("```\n");
    s.push_str(sentinel);
    s.push_str(" — scroll payload follows.\n");
    for i in 0..rows {
        s.push_str(&format!("line {i} payload\n"));
    }
    s.push_str("```\n");
    s
}

// ── Fake session-auth (OAuth) seeding ───────────────────────────────────
// `seed_fake_oauth` / `oauth_credential_ops` live in `distill_pager_pty_harness::flows` (re-exported above)

/// The harness's default `XAI_API_KEY` (ApiKey/BYOK mode, no auth.json entry) would never fetch
/// `/v1/settings`. Spawns WITHOUT `GROK_ANNOUNCEMENTS_OVERRIDE` (the env override beats pushed
/// lists in the pager and would mask updates).
pub(crate) fn spawn_polling_session(content: &ContentController, oauth_user: &str) -> PtyHarness {
    spawn_polling_session_with_env(content, oauth_user, &[])
}

/// [`spawn_polling_session`] with extra env pairs appended after the shared oauth/poll env (e.g. a test-override path or a `TERM_PROGRAM` pin).
pub(crate) fn spawn_polling_session_with_env(
    content: &ContentController,
    oauth_user: &str,
    extra_env: &[(&str, &str)],
) -> PtyHarness {
    seed_fake_oauth(content, oauth_user);
    let mut overrides = Vec::from(oauth_credential_ops());
    overrides.push(EnvOp::set("GROK_ANNOUNCEMENTS_REFRESH_INTERVAL_SECS", "1"));
    overrides.extend(extra_env.iter().map(|(key, value)| EnvOp::set(key, value)));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_ops_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        content,
        &[],
        &overrides,
        Some(content.home()),
    )
    .expect("spawn pager with polling session auth");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt to enter session");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("session response");
    harness
}

// ── Agent type mismatch e2e tests ──────────────────────────────────────

/// Start the mock server with two models that have different agent types.
/// Returns a `ContentController` configured for agent-type-mismatch testing.
/// The default model is `"default-model"` (no agent type, so it uses the `distill` harness).
pub(crate) async fn start_dual_agent_type_content() -> ContentController {
    ContentController::start_with_models(vec![
        MockModel::new("default-model"),
        MockModel::with_agent_type("cursor-model", "cursor"),
    ])
    .await
    .expect("start content with dual agent types")
}

// ── Folder-trust welcome sub-state e2e ──────────────────────────────────

/// Title line of the folder-trust question (see `render_welcome_trust`).
pub(crate) const TRUST_QUESTION_SENTINEL: &str = "Do you trust the contents of this directory";

/// A `git init`'d temp dir containing a repo-local `.mcp.json` (code-exec config).
/// `repo_configs_present` returns true for it, so an untrusted clone with the feature on resolves to `Prompt` and the trust question renders.
pub(crate) fn git_repo_with_mcp_json() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo tempdir");
    git2::Repository::init(repo.path()).expect("git init");
    std::fs::write(repo.path().join(".mcp.json"), "{}").expect("write .mcp.json");
    repo
}

/// Explicit overrides for a folder-trust run.
/// A self-built grok auto-trusts, so `GROK_TEST_VERSION` simulates a release; the gate is pinned both ways.
pub(crate) fn trust_env(feature_on: bool) -> [(&'static str, &'static str); 2] {
    [
        ("GROK_TEST_VERSION", "0.0.0-sim"),
        ("GROK_FOLDER_TRUST", if feature_on { "1" } else { "0" }),
    ]
}

/// Filename of the folder-trust store under `$HOME/.grok`. Mirrors
/// `distill_workspace::trust::TRUST_FILE_NAME`; the harness does not link the workspace crate.
pub(crate) const TRUST_FILE_NAME: &str = "trusted_folders.toml";

/// Whether the `trusted_folders.toml` at `store_path` records a grant covering `query`.
/// Reads the on-disk document (`[folders."<canonical path>"] trusted = <bool>`) the way
/// `TrustStore::is_trusted` does: the most specific recorded ancestor of the canonical query decides.
pub(crate) fn store_trusts(store_path: &Path, query: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(store_path) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml::Table>() else {
        return false;
    };
    let Some(folders) = doc.get("folders").and_then(toml::Value::as_table) else {
        return false;
    };
    let query = dunce::canonicalize(query).unwrap_or_else(|_| query.to_path_buf());
    folders
        .iter()
        .filter(|(folder, _)| query.starts_with(folder))
        .max_by_key(|(folder, _)| Path::new(folder).components().count())
        .is_some_and(|(_, record)| {
            record.get("trusted").and_then(toml::Value::as_bool) == Some(true)
        })
}

/// Whether the isolated trust store has recorded a grant covering `repo` (a repo root, so its own workspace key).
pub(crate) fn folder_is_trusted(content: &ContentController, repo: &Path) -> bool {
    store_trusts(&content.home().join(".grok").join(TRUST_FILE_NAME), repo)
}

// Leader mode e2e. The leader cluster cases moved to the dedicated `tests/leader_pty_e2e` target.
// Their LEADER_TIMEOUT/STREAM_TIMEOUT/submit_turn/inference_request_count helpers moved with them.
// Only the helpers non-leader tests still use remain here.

/// Sentinel for turn `n`, short enough to never wrap at 120 cols (wrapping would break the exactly-once occurrence counts).
pub(crate) fn turn_sentinel(n: u8) -> String {
    format!("{MOCK_RESPONSE_SENTINEL}_T{n}")
}

// `wait_for_labels_absent` lives in `distill_pager_pty_harness::flows` (re-exported above)

// ── MCP menu loading e2e tests ──────────────────────────────────────────

/// Seeded server name; it only renders once the MCP list fetch resolves.
pub(crate) const MCP_TEST_SERVER: &str = "ptytestmcp";

/// Budget for session creation plus the `x.ai/mcp/list` round-trip.
pub(crate) const MCP_MENU_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Configured servers list with a status badge even when never connected.
pub(crate) fn seed_mcp_server_config(content: &ContentController) {
    #[cfg(not(windows))]
    let command = "/bin/cat";
    #[cfg(windows)]
    let command = "cmd.exe";

    let distill_home = content.home().join(".grok");
    std::fs::create_dir_all(&distill_home).expect("create fake GROK_HOME");
    let config = format!(
        "[mcp_servers.{MCP_TEST_SERVER}]\ncommand = \"{command}\"\nargs = []\nstartup_timeout_sec = 2\n"
    );
    std::fs::write(distill_home.join("config.toml"), config).expect("write config.toml");
}

/// Write one hooks spec file under the sandbox's `~/.grok/hooks/` (`spec` is the file's JSON body).
pub(crate) fn seed_hook_spec(
    content: &ContentController,
    file_name: &str,
    spec: &serde_json::Value,
) {
    let hooks_dir = content.home().join(".grok").join("hooks");
    std::fs::create_dir_all(&hooks_dir).expect("create ~/.grok/hooks");
    std::fs::write(
        hooks_dir.join(file_name),
        serde_json::to_vec_pretty(spec).expect("serialize hook spec"),
    )
    .expect("write hook spec");
}

/// Spawn the pager in `cwd`, open `/mcps`, wait for the seeded server.
pub(crate) async fn drive_mcp_menu_load(content: &ContentController, cwd: &std::path::Path) {
    seed_mcp_server_config(content);

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        content,
        &[],
        Some(cwd),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    // Typing `/` promotes the welcome screen into a new session first.
    harness.inject_keys(b"/mcps\r").expect("submit /mcps");

    // Tab chrome renders even while loading, so this gate isolates setup failures.
    harness
        .wait_for_text("MCP Servers", Duration::from_secs(15))
        .expect("extensions modal open on MCP Servers tab");

    harness
        .wait_for_text(MCP_TEST_SERVER, MCP_MENU_LOAD_TIMEOUT)
        .expect("MCP server list loaded in menu");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}

// ── Queue + interjection e2e tests ──────────────────────────────────────

// Ctrl+Enter / Ctrl+; as CSI-u, parsed without kitty protocol negotiation.
pub(crate) const CTRL_ENTER: &[u8] = b"\x1b[13;5u";

pub(crate) const CTRL_SEMICOLON: &[u8] = b"\x1b[59;5u";

/// Ctrl+\ (OpenDashboard). crossterm maps the raw 0x1c byte to Ctrl+4, so the chord must be sent as kitty CSI-u: code 92 (`\`), modifier 5 (Ctrl).
pub(crate) const CTRL_BACKSLASH: &[u8] = b"\x1b[92;5u";

/// Wire prefix the shell puts on interjected messages.
pub(crate) const INTERJECTION_WIRE_PREFIX: &str = "The user sent a message while you were working";

/// ~5s of paced streaming so the test can type during the turn.
pub(crate) fn slow_turn_text(sentinel: &str) -> String {
    let mut s = String::from(sentinel);
    for i in 0..30 {
        s.push_str(&format!(" streaming{i}"));
    }
    s
}

/// All user-message contents across every recorded request, in order.
pub(crate) fn all_user_messages(content: &ContentController) -> Vec<String> {
    content
        .request_bodies()
        .iter()
        .flat_map(|b| {
            b["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"] == "user")
                .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Visible screen lines showing `text` INSIDE the bordered composer (the prompt-box row carries a `│` border; committed scrollback lines don't).
/// Keep needles short enough not to wrap at [`DEFAULT_COLS`].
pub(crate) fn composer_holds(harness: &PtyHarness, text: &str) -> bool {
    harness
        .screen_contents()
        .lines()
        .any(|l| l.contains('│') && l.contains(text))
}

/// Count of visible screen lines showing `text` OUTSIDE the bordered composer (committed scrollback copies).
/// The cancel/rewind duplicate-render regression tests assert this count is exactly one.
pub(crate) fn block_lines_containing(harness: &PtyHarness, text: &str) -> usize {
    harness
        .screen_contents()
        .lines()
        .filter(|l| l.contains(text) && !l.contains('│'))
        .count()
}

/// 19b. **VS Code family: Ctrl+L (form feed)** is the send-now chord, behaving like the default Ctrl+Enter binding.
/// Harness strips `TERM_PROGRAM` then applies env; pass `vscode` so defaults bind the chord to Ctrl+L.
pub(crate) const CTRL_L: &[u8] = b"\x0c";

/// Ctrl+O (C0 0x0F).
/// On Apple Terminal this is the InterjectPrompt / send-now chord.
/// In minimal mode it also doubles as the transcript-pager remap when interject would no-op.
pub(crate) const CTRL_O: &[u8] = b"\x0f";

// NOTE: There is no SessionStart hook exactly-once e2e test
// Deduplication in load_hooks_from_sources is covered by unit tests in distill-hooks::discovery::tests
// A PTY e2e test would need careful environment variable setup to avoid static caching issues with GROK_HOME

// ── Mouse reporting toggle (opt-in scrollback Ctrl+R) ───────────────────

/// Sticky banner shown while mouse reporting is off (must match pager copy).
pub(crate) const MOUSE_OFF_STICKY: &str =
    "Ctrl+r to enable mouse reporting and restore TUI features";

/// Prompt-focused form of the same sticky (swap at render time in `active_toast_message`).
pub(crate) const MOUSE_OFF_HINT_PROMPT: &str =
    "/toggle-mouse-reporting to enable mouse reporting and restore TUI features";

/// Seed `~/.grok/config.toml` with a `[ui]` section body (e.g. `"vim_mode = true"`).
/// Same `{GROK_HOME|HOME}/.grok/config.toml` location `seed_mouse_reporting_toggle_config` uses; call before spawning the pager.
pub(crate) fn seed_ui_config(content: &ContentController, ui_body: &str) {
    let distill_home = content.home().join(".grok");
    std::fs::create_dir_all(&distill_home).expect("create .grok");
    let config = format!("[ui]\n{ui_body}\n");
    std::fs::write(distill_home.join("config.toml"), config).expect("write config.toml");
}

pub(crate) fn seed_mouse_reporting_toggle_config(content: &ContentController, enabled: bool) {
    let distill_home = content.home().join(".grok");
    std::fs::create_dir_all(&distill_home).expect("create .grok");
    // Minimal opt-in only; matches load_config's `{GROK_HOME|HOME}/.grok/config.toml`
    let config = if enabled {
        "[ui]\nmouse_reporting_toggle = true\n"
    } else {
        // Minimal config so HOME layout matches the enabled case; toggle stays off.
        "[ui]\n"
    };
    std::fs::write(distill_home.join("config.toml"), config).expect("write config.toml");
}

/// Seed `[ui] keep_text_selection = "hold"` under the content controller's home.
pub(crate) fn seed_keep_text_selection_config(content: &ContentController) {
    let distill_home = content.home().join(".grok");
    std::fs::create_dir_all(&distill_home).expect("create .grok");
    std::fs::write(
        distill_home.join("config.toml"),
        "[ui]\nkeep_text_selection = \"hold\"\n",
    )
    .expect("write config.toml");
}

/// Spawn pager with the mouse-toggle opt-in after the sandbox baseline.
pub(crate) fn spawn_mouse_toggle_pager(content: &ContentController) -> PtyHarness {
    let binary = pager_binary().expect("resolve pager binary");
    PtyHarness::spawn_with_content_env_ops(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        content,
        &[],
        &[EnvOp::set("GROK_MOUSE_REPORTING_TOGGLE", "true")],
    )
    .expect("spawn pager")
}

/// Inject keys one byte at a time with a short drain between each so the pager event loop processes them as discrete key events.
/// Bulk injects (especially post-turn, when no steady `tracing_rx` tick wakes non-dev builds) can arrive in a single `EventStream` batch.
/// They then get paste-coalesced (`[Pasted: N lines]`), which never reaches slash submit.
pub(crate) fn inject_keys_paced(harness: &mut PtyHarness, keys: &[u8]) {
    for &b in keys {
        harness.inject_keys(&[b]).expect("inject paced key");
        harness.update(Duration::from_millis(50));
    }
}

/// Widens the pager's idle-Esc double-press window (bounded by `esc_double_press_ttl` in `app_view.rs`).
/// On a loaded shard, the render round-trip between the two presses must not expire the pending first press.
pub(crate) const ESC_DOUBLE_PRESS_ENV: &str = "GROK_ESC_DOUBLE_PRESS_MS";

/// Spawn the pager with [`ESC_DOUBLE_PRESS_ENV`] set to the 60s cap
/// (`distill_pager::app::app_view::ESC_DOUBLE_PRESS_TEST_MS`; the pager clamps larger values to it).
pub(crate) fn spawn_esc_double_press_pager(content: &ContentController) -> PtyHarness {
    let binary = pager_binary().expect("resolve pager binary");
    PtyHarness::spawn_with_content_env_ops(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        content,
        &[],
        &[EnvOp::set(ESC_DOUBLE_PRESS_ENV, "60000")],
    )
    .expect("spawn pager")
}

/// Reach an agent session with scrollback content, then focus scrollback (Tab).
pub(crate) async fn drive_to_scrollback_with_turn(
    harness: &mut PtyHarness,
    content: &ContentController,
) {
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} mouse toggle turn."));
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("turn rendered");
    // Leave the prompt so scrollback-only Ctrl+R can fire (unbound on the prompt).
    // Tab is the leave-prompt / focus-scrollback key (Esc is reserved for the cancel / clear / rewind policy)
    harness.inject_keys(b"\t").expect("focus scrollback (tab)");
    harness.update(Duration::from_millis(500));
    // Footer shows "Space:prompt" when scrollback owns keys (prompt is not focused).
    let _ = harness.wait_for_text("Space:prompt", Duration::from_secs(5));
}

// ── Tool header selection (path/command operand only) ───────────────────

/// Unique filename marker so the Read header is unambiguous on screen.
pub(crate) const READ_HDR_FILE: &str = "read_hdr_sel_target.txt";

pub(crate) const READ_HDR_SENTINEL: &str = "READ_HDR_SEL_DONE";

/// Responses API SSE stream that emits a single `function_call` tool invoke.
pub(crate) fn responses_api_tool_call_events(
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut seq = 0u64;
    events.push(SseEvent::data(
        json!({
            "type": "response.created",
            "sequence_number": seq,
            "response": {
                "id": "resp_read_hdr",
                "object": "response",
                "created_at": 1234567890,
                "model": "test-model",
                "status": "in_progress",
                "output": []
            }
        })
        .to_string(),
    ));
    seq += 1;
    events.push(SseEvent::data(
        json!({
            "type": "response.function_call_arguments.delta",
            "sequence_number": seq,
            "item_id": call_id,
            "output_index": 0,
            "delta": arguments
        })
        .to_string(),
    ));
    seq += 1;
    events.push(SseEvent::data(
        json!({
            "type": "response.completed",
            "sequence_number": seq,
            "response": {
                "id": "resp_read_hdr",
                "object": "response",
                "created_at": 1234567890,
                "model": "test-model",
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments
                }],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "total_tokens": 30,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 0 }
                }
            }
        })
        .to_string(),
    ));
    events.push(SseEvent::data("[DONE]".to_string()));
    events
}

/// [`chat_completions_tool_call_events`] with an explicit `tool_call` id, for tests scripting several calls into ONE conversation.
/// A reused id would alias distinct calls in history and confuse dangling-call bookkeeping.
pub(crate) fn chat_completions_tool_call_events_with_id(
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Vec<SseEvent> {
    let tool_calls = vec![json!({
        "index": 0,
        "id": call_id,
        "type": "function",
        "function": { "name": name, "arguments": arguments }
    })];
    vec![
        SseEvent::data(
            json!({
                "id": "chatcmpl-read-hdr",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": tool_calls
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
        ),
        SseEvent::data(
            json!({
                "id": "chatcmpl-read-hdr",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 20,
                    "total_tokens": 30
                }
            })
            .to_string(),
        ),
        SseEvent::data("[DONE]".to_string()),
    ]
}

/// Poll the raw PTY stream until at least one OSC 52 clipboard payload is flushed (or `timeout` elapses), then return everything decoded so far.
/// A copy lands asynchronously after the triggering input, so a fixed post-release sleep flakes under CI/host load.
pub(crate) fn wait_for_osc52_payloads(harness: &mut PtyHarness, timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    loop {
        harness.update(Duration::from_millis(200));
        let payloads = decode_osc52_payloads(harness.raw_output());
        if !payloads.is_empty() || Instant::now() >= deadline {
            return payloads;
        }
    }
}

pub(crate) fn decode_osc52_payloads(bytes: &[u8]) -> Vec<String> {
    use base64::Engine as _;
    let output = String::from_utf8_lossy(bytes);
    let mut payloads = Vec::new();
    for segment in output.split("\x1b]52;").skip(1) {
        let Some((_, rest)) = segment.split_once(';') else {
            continue;
        };
        // No BEL/ST terminator yet means a mid-flush tail
        // Skip it so the poll in wait_for_osc52_payloads waits for the complete payload instead of decoding a truncated prefix that still parses as base64
        let Some(end) = rest.find(['\x07', '\x1b']) else {
            continue;
        };
        let encoded = &rest[..end];
        if encoded.is_empty() {
            continue;
        }
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded)
            && let Ok(text) = String::from_utf8(decoded)
        {
            payloads.push(text);
        }
    }
    payloads
}

/// One SGR (DECSET 1006) mouse report for button `btn` at 0-based (row,col), emitted as the 1-based SGR wire encoding:
/// `suffix` 'M' means press/motion and 'm' means release; `btn` carries the +32 motion bit and the +64 wheel bit.
/// Encoding spec: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-Mouse-Tracking
pub(crate) fn sgr_mouse(btn: u16, row: u16, col: u16, suffix: char) -> String {
    format!("\x1b[<{btn};{};{}{suffix}", col + 1, row + 1)
}

/// SGR mouse drag from (row,col) inclusive to (row,end_col) inclusive.
pub(crate) fn mouse_drag_line(row: u16, from_col: u16, to_col: u16) -> String {
    let mut out = String::new();
    out.push_str(&sgr_mouse(0, row, from_col, 'M')); // press
    out.push_str(&sgr_mouse(32, row, (from_col + to_col) / 2, 'M')); // drag mid
    out.push_str(&sgr_mouse(32, row, to_col, 'M')); // drag end
    out.push_str(&sgr_mouse(0, row, to_col, 'm')); // release
    out
}

/// SGR mouse press and drag from (row,from_col) to (row,to_col) inclusive with no final release.
pub(crate) fn mouse_drag_no_release(row: u16, from_col: u16, to_col: u16) -> String {
    let mut out = String::new();
    out.push_str(&sgr_mouse(0, row, from_col, 'M')); // press
    out.push_str(&sgr_mouse(32, row, (from_col + to_col) / 2, 'M')); // drag mid
    out.push_str(&sgr_mouse(32, row, to_col, 'M')); // drag end
    out
}

pub(crate) fn locate_screen_text(screen: &str, needle: &str) -> Option<(u16, u16)> {
    for (row, line) in screen.lines().enumerate() {
        if let Some(byte) = line.find(needle) {
            let col = line[..byte].chars().count() as u16;
            return Some((row as u16, col));
        }
    }
    None
}

/// Register one named scripted tool-call turn on both inference endpoints.
pub(crate) fn expect_tool_turn(
    content: &ContentController,
    call_id: &str,
    name: &str,
    args: String,
) -> AgentTurnExpectation {
    content.expect_agent_turn_with_responses(
        format!("tool turn {call_id}"),
        ScriptedResponse::sse(responses_api_tool_call_events(call_id, name, &args)),
        ScriptedResponse::sse(chat_completions_tool_call_events_with_id(
            call_id, name, &args,
        )),
    )
}

/// Responses API SSE stream whose `response.completed` output carries one `function_call` item per entry of `calls`.
/// That is a single model turn invoking parallel tool calls.
/// Each entry is `(call_id, name, arguments)`; ids must be distinct or history bookkeeping aliases the calls.
pub(crate) fn responses_api_parallel_tool_call_events(
    calls: &[(&str, &str, String)],
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut seq = 0u64;
    events.push(SseEvent::data(
        json!({
            "type": "response.created",
            "sequence_number": seq,
            "response": {
                "id": "resp_parallel_tools",
                "object": "response",
                "created_at": 1234567890,
                "model": "test-model",
                "status": "in_progress",
                "output": []
            }
        })
        .to_string(),
    ));
    for (i, (call_id, _name, arguments)) in calls.iter().enumerate() {
        seq += 1;
        events.push(SseEvent::data(
            json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": seq,
                "item_id": call_id,
                "output_index": i,
                "delta": arguments
            })
            .to_string(),
        ));
    }
    seq += 1;
    let output: Vec<serde_json::Value> = calls
        .iter()
        .map(|(call_id, name, arguments)| {
            json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments
            })
        })
        .collect();
    events.push(SseEvent::data(
        json!({
            "type": "response.completed",
            "sequence_number": seq,
            "response": {
                "id": "resp_parallel_tools",
                "object": "response",
                "created_at": 1234567890,
                "model": "test-model",
                "status": "completed",
                "output": output,
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "total_tokens": 30,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 0 }
                }
            }
        })
        .to_string(),
    ));
    events.push(SseEvent::data("[DONE]".to_string()));
    events
}

/// Chat Completions twin of [`responses_api_parallel_tool_call_events`].
/// One chunk whose `delta.tool_calls` carries every call (index 0, 1, …), then a `finish_reason: "tool_calls"` chunk.
pub(crate) fn chat_completions_parallel_tool_call_events(
    calls: &[(&str, &str, String)],
) -> Vec<SseEvent> {
    let tool_calls: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (call_id, name, arguments))| {
            json!({
                "index": i,
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            })
        })
        .collect();
    vec![
        SseEvent::data(
            json!({
                "id": "chatcmpl-parallel-tools",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": tool_calls
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
        ),
        SseEvent::data(
            json!({
                "id": "chatcmpl-parallel-tools",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 20,
                    "total_tokens": 30
                }
            })
            .to_string(),
        ),
        SseEvent::data("[DONE]".to_string()),
    ]
}

/// Queue one scripted turn with parallel tool calls on both inference endpoints (see [`expect_tool_turn`]).
pub(crate) fn enqueue_parallel_tool_turn(
    content: &ContentController,
    calls: &[(&str, &str, String)],
) {
    content.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_parallel_tool_call_events(calls)),
    );
    content.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::sse(chat_completions_parallel_tool_call_events(calls)),
    );
}

/// Seed a target file under the isolated HOME and queue a scripted `read_file` tool call (Responses and Chat Completions).
/// The pager then renders a Read header.
pub(crate) fn seed_read_file_tool_call(
    content: &ContentController,
    abs_path: &Path,
) -> AgentTurnExpectation {
    let args = json!({ "target_file": abs_path.to_string_lossy() }).to_string();
    let turn = expect_tool_turn(content, "call_read_hdr", "read_file", args);
    // Follow-up turn after tool result: plain completion so the session settles.
    content.set_response(READ_HDR_SENTINEL);
    turn
}

// ── Minimal (scrollback-native) mode e2e helpers ────────────────────────

/// Args that launch the pager in the experimental scrollback-native minimal mode, standalone.
/// Minimal is single-session (no leader/multi-client), so `--no-leader` keeps the test off the shared-daemon path.
pub(crate) const MINIMAL_ARGS: &[&str] = &["--minimal", "--no-leader"];

/// Idle status-line text minimal renders at the prompt (see `crate::minimal::live::render_status`).
/// Distinct from the running status (`working…`), so it doubles as a "ready / turn finished" sentinel.
pub(crate) const MINIMAL_IDLE_SENTINEL: &str = "minimal · /help";

/// Idle status after a slash `/minimal` re-exec (switch-back cue present).
/// Distinct from [`MINIMAL_IDLE_SENTINEL`]: cold `--minimal` starts omit the reverse-command segment.
pub(crate) const MINIMAL_SWITCH_BACK_IDLE_SENTINEL: &str =
    "minimal · /fullscreen to go back · /help";

/// Header of minimal's parked plan-approval controls strip.
pub(crate) const PLAN_PARKED_SENTINEL: &str = "Plan ready for review";

/// A plan body the `exit_plan_mode` tool will read off disk.
/// Every step carries a unique `{tag}{NNN}` sentinel, because a truncated plan still contains its head and would pass a plain substring check.
pub(crate) fn plan_body(tag: &str, lines: usize) -> String {
    let mut s = format!("# {tag} Plan\n\n");
    for i in 0..lines {
        s.push_str(&format!("- {tag}{i:03} step of the plan\n"));
    }
    s
}

/// Steps of a [`plan_body`] missing from everything the user could reach by scrolling: native scrollback plus the visible screen.
pub(crate) fn plan_lines_missing(harness: &mut PtyHarness, tag: &str, lines: usize) -> Vec<usize> {
    let full = harness.full_text();
    (0..lines)
        .filter(|i| !full.contains(&format!("{tag}{i:03}")))
        .collect()
}

/// Steps of a [`plan_body`] that appear more than once: the print-once guard.
pub(crate) fn plan_lines_duplicated(
    harness: &mut PtyHarness,
    tag: &str,
    lines: usize,
) -> Vec<usize> {
    let full = harness.full_text();
    (0..lines)
        .filter(|i| full.matches(&format!("{tag}{i:03}")).count() > 1)
        .collect()
}

/// Drive a full-TUI pager from the welcome screen into a bound session (prompt sent, mock response rendered).
pub(crate) fn enter_session(harness: &mut PtyHarness, content: &ContentController) {
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} session ready."));
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("session response rendered");
}

/// Locate `<distill_home>/sessions/<encoded cwd>/<session id>/`, where the shell keeps the session's `plan.md`.
/// Polls: the first turn creates it asynchronously.
pub(crate) fn session_dir(content: &ContentController, harness: &mut PtyHarness) -> PathBuf {
    let sessions = content.home().join(".grok").join("sessions");
    for _ in 0..100 {
        if let Ok(outer) = std::fs::read_dir(&sessions) {
            for cwd_dir in outer.flatten() {
                if let Ok(inner) = std::fs::read_dir(cwd_dir.path()) {
                    for entry in inner.flatten() {
                        if entry.path().is_dir() {
                            return entry.path();
                        }
                    }
                }
            }
        }
        harness.update(Duration::from_millis(100));
    }
    panic!("no session dir under {}", sessions.display());
}

/// The session's saved feedback drafts (`feedback_drafts.json` → `drafts`); empty only while the file does not exist yet.
pub(crate) fn read_feedback_drafts(session_dir: &Path) -> Vec<serde_json::Value> {
    let path = session_dir.join("feedback_drafts.json");
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("read {}: {error}", path.display()),
    };
    serde_json::from_slice::<serde_json::Value>(&raw)
        .expect("feedback_drafts.json is JSON")
        .get("drafts")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .expect("feedback_drafts.json has a drafts array")
}

/// Set a pager up to really POST feedback: writes `user`'s fake OAuth into the sandbox home (an API key alone
/// makes the shell keep the report local) and returns the env ops that open the shell gate the sandbox baseline
/// closes.
pub(crate) fn enable_feedback_posting(
    content: &ContentController,
    user: &str,
) -> Vec<EnvOp<'static>> {
    seed_fake_oauth(content, user);
    let mut ops = Vec::from(oauth_credential_ops());
    ops.push(EnvOp::set("GROK_FEEDBACK_ENABLED", "true"));
    ops
}

/// The Write tab's empty-composer placeholder: the one string that says the feedback form is open on Write.
pub(crate) const FEEDBACK_MODAL_PLACEHOLDER_SENTINEL: &str = "Tell us what happened";

/// Open the feedback form via bare `/feedback` and wait for its Write tab. Paced: a single-shot burst can
/// land Enter before the composer absorbed the slash filter under remote CI load.
pub(crate) fn open_feedback_modal(harness: &mut PtyHarness) {
    inject_keys_paced(harness, b"/feedback");
    harness.inject_keys(b"\r").expect("submit /feedback");
    harness
        .wait_for_text(FEEDBACK_MODAL_PLACEHOLDER_SENTINEL, Duration::from_secs(15))
        .expect("feedback modal composer should render");
}

/// Wait for the mock to record the send whose `structured_feedback.source` is `source` (the mock records failed
/// POSTs too), then return its body; a second post with that source would mean a double send.
pub(crate) fn wait_for_feedback_post(
    harness: &mut PtyHarness,
    content: &ContentController,
    source: &str,
) -> serde_json::Value {
    let posts_from = |content: &ContentController| -> Vec<serde_json::Value> {
        content
            .feedback_posts()
            .into_iter()
            .map(|post| post.body)
            .filter(|body| body["metadata"]["structured_feedback"]["source"] == source)
            .collect()
    };
    harness
        .wait_until(
            &format!("{source} feedback POST recorded"),
            Duration::from_secs(15),
            |_| !posts_from(content).is_empty(),
        )
        .expect("the send reaches the mock feedback route");
    let mut posts = posts_from(content);
    assert_eq!(
        1,
        posts.len(),
        "exactly one {source} POST per send: {posts:?}"
    );
    posts.remove(0)
}

/// The whole draft-send contract, shared by the minimal and full-TUI cases. Precondition: an idle, bound session
/// (`session_dir` resolved) with feedback enabled and OAuth seeded ([`enable_feedback_posting`]), and no drafts yet.
/// The draft is seeded by a scripted failure: the `/feedback <report>` POST fails and the text is kept as one
/// typeless predraft with a "Saved to Drafts" notice. Bare `/feedback` then opens it from Drafts, a typeless send
/// is refused, a type is picked and the send lands as a second `POST /v1/feedback` with `source == "draft"`,
/// after which the drafts file is empty. `Right` from an unset Type lands on `Idea` because the picker treats
/// unset as index 0 (`Bug`) and steps once. No trace step intercepts the full-TUI Enter: the sandbox baseline
/// leaves the trace card feature and telemetry off.
pub(crate) fn drive_draft_send(
    harness: &mut PtyHarness,
    content: &ContentController,
    session_dir: &Path,
    report: &str,
) {
    content.set_feedback_failure(true);
    inject_keys_paced(harness, format!("/feedback {report}").as_bytes());
    harness.inject_keys(b"\r").expect("submit inline /feedback");
    // The notice trails a long error, so it wraps at any width: match it with the line breaks collapsed.
    harness
        .wait_until("failed inline send kept", Duration::from_secs(30), |h| {
            h.full_text()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("Saved to Drafts")
        })
        .expect("a failed inline send is kept as a draft");
    // The on-disk state a failed `/feedback <report>` send leaves behind: one draft, no type.
    let drafts = read_feedback_drafts(session_dir);
    assert_eq!(1, drafts.len(), "one predraft on disk: {drafts:?}");
    assert_eq!(report, drafts[0]["details"]);
    assert!(
        drafts[0].get("type").is_none(),
        "a predraft carries no type: {drafts:?}"
    );
    content.set_feedback_failure(false);

    let draft_row = format!("Unclassified · Other · {report}");
    inject_keys_paced(harness, b"/feedback");
    harness.inject_keys(b"\r").expect("submit bare /feedback");
    harness
        .wait_for_text(&draft_row, Duration::from_secs(15))
        .expect("bare /feedback opens on the Drafts tab with the predraft row");
    harness.inject_keys(b"\r").expect("load the selected draft");
    harness
        .wait_for_text("Type: (choose)", Duration::from_secs(15))
        .expect("the loaded predraft shows its unset Type row");
    harness.inject_keys(b"\r").expect("send the typeless draft");
    harness
        .wait_for_text(
            "Choose a type before sending this draft.",
            Duration::from_secs(15),
        )
        .expect("a typeless draft is refused in the modal");
    harness.inject_keys(b"\t").expect("focus the Type row");
    harness.inject_keys(keys::RIGHT).expect("cycle Type");
    harness
        .wait_for_text("Type: Idea", Duration::from_secs(15))
        .expect("Right from an unset Type lands on Idea");
    harness.inject_keys(b"\t").expect("back to the composer");
    harness.inject_keys(b"\r").expect("send the typed draft");
    harness
        .wait_until("draft modal closed", Duration::from_secs(30), |h| {
            !h.contains_text("Type: Idea")
        })
        .expect("a successful draft send closes the modal");

    let body = wait_for_feedback_post(harness, content, "draft");
    let posts = content.feedback_posts();
    assert_eq!(
        2,
        posts.len(),
        "the failed inline send and the draft send: {posts:?}"
    );
    assert!(
        body["feedbackText"]
            .as_str()
            .is_some_and(|text| text.contains(report)),
        "POST body: {body}"
    );
    assert_eq!("tui", body["clientType"], "POST body: {body}");
    assert_eq!(
        "idea", body["metadata"]["structured_feedback"]["type"],
        "POST body: {body}"
    );
    harness
        .wait_until("sent draft deleted", Duration::from_secs(15), |_| {
            read_feedback_drafts(session_dir).is_empty()
        })
        .expect("a sent draft leaves the drafts file");
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
}

/// Spawn the pager in minimal mode against `content` at the default size.
pub(crate) fn spawn_minimal(content: &ContentController) -> PtyHarness {
    spawn_minimal_sized(content, DEFAULT_ROWS, DEFAULT_COLS)
}

/// Spawn minimal at an explicit terminal size.
pub(crate) fn spawn_minimal_sized(content: &ContentController, rows: u16, cols: u16) -> PtyHarness {
    spawn_minimal_env_ops(content, rows, cols, &[], &[], None)
}

/// Spawn minimal in an explicit project dir, appending `extra_args` to [`MINIMAL_ARGS`] (e.g.
/// `--continue`).
pub(crate) fn spawn_minimal_in_dir(
    content: &ContentController,
    rows: u16,
    cols: u16,
    extra_args: &[&str],
    cwd: &Path,
) -> PtyHarness {
    spawn_minimal_env_ops(content, rows, cols, extra_args, &[], Some(cwd))
}

/// The one minimal spawn: [`MINIMAL_ARGS`] plus `extra_args`, sandbox env overrides (e.g. `GROK_FEEDBACK_ENABLED`
/// for the shell gates the sandbox baseline closes), and an optional project dir.
pub(crate) fn spawn_minimal_env_ops(
    content: &ContentController,
    rows: u16,
    cols: u16,
    extra_args: &[&str],
    ops: &[EnvOp<'_>],
    cwd: Option<&Path>,
) -> PtyHarness {
    let binary = pager_binary().expect("resolve pager binary");
    let mut args = MINIMAL_ARGS.to_vec();
    args.extend_from_slice(extra_args);
    let mut harness = PtyHarness::spawn_with_content_env_ops_in_dir(
        &binary, rows, cols, content, &args, ops, cwd,
    )
    .expect("spawn minimal pager with env ops");
    // Without the query responder `--minimal` silently downgrades (the terminal probe times out).
    harness.set_respond_to_queries(true);
    harness
}

/// Block until minimal has cold-started into its agent session and is idle at the prompt (the `minimal · /help` status line is showing).
/// Minimal has no welcome screen, so this, not [`WELCOME_SCREEN_SENTINEL`], is the readiness gate.
pub(crate) fn wait_minimal_ready(harness: &mut PtyHarness) {
    harness
        .wait_for_text(MINIMAL_IDLE_SENTINEL, WELCOME_TIMEOUT)
        .unwrap_or_else(|e| {
            panic!(
                "minimal never cold-started to an idle prompt: {e}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
}

/// Bind an idle minimal pager to a session: prompt sent, mock response rendered, idle status back.
/// Bare `/feedback` (and every session-scoped command) refuses until this has happened.
pub(crate) fn bind_minimal_session(harness: &mut PtyHarness, content: &ContentController) {
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} minimal ready."));
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("response rendered");
    harness
        .wait_for_text(MINIMAL_IDLE_SENTINEL, Duration::from_secs(30))
        .expect("turn finished");
}

/// Quit minimal cleanly. The prompt is always focused (a bare `q` would type into it), so quit is
/// Ctrl+Q pressed twice (it requires confirmation). Falls back to the harness kill path if the
/// chord doesn't take.
pub(crate) fn quit_minimal(harness: &mut PtyHarness) {
    let _ = harness.inject_keys(b"\x11"); // Ctrl+Q, first press (quit needs confirmation)
    harness.update(Duration::from_millis(200));
    let _ = harness.inject_keys(b"\x11"); // Ctrl+Q, second press confirms
    match harness
        .wait_exit_code(Duration::from_secs(15))
        .expect("wait for minimal pager exit")
    {
        PtyExitPoll::Running => harness.quit().expect("kill minimal pager after timeout"),
        PtyExitPoll::Exited(_) | PtyExitPoll::PendingStatus => {}
    }
}

/// Write a local non-interactive editor script into `dir` and return the quoted command to hand to `$VISUAL`.
/// Both bodies receive the draft path as their first argument.
pub(crate) fn fake_editor_command(
    dir: &std::path::Path,
    unix_body: &str,
    windows_body: &str,
) -> String {
    if cfg!(windows) {
        let script = dir.join("local-editor.cmd");
        std::fs::write(&script, windows_body).expect("write Windows editor script");
        format!("cmd /C '{}'", script.display())
    } else {
        let script = dir.join("local-editor.sh");
        std::fs::write(&script, unix_body).expect("write Unix editor script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .expect("make editor executable");
        }
        format!("'{}'", script.display())
    }
}

const EXIT_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn resolve_exit_status_poll<T, E>(
    poll: Result<PtyExitPoll<T>, E>,
    deadline_reached: bool,
) -> Result<Option<PtyExitPoll<T>>, E> {
    match poll? {
        PtyExitPoll::Exited(code) => Ok(Some(PtyExitPoll::Exited(code))),
        state if deadline_reached => Ok(Some(state)),
        PtyExitPoll::Running | PtyExitPoll::PendingStatus => Ok(None),
    }
}

/// Wait for a concrete exit status while preserving the typed deadline state.
pub(crate) fn wait_for_exit_status(
    harness: &mut PtyHarness,
    timeout: Duration,
) -> anyhow::Result<PtyExitPoll<u32>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(state) = resolve_exit_status_poll(
            harness.wait_exit_code(Duration::ZERO),
            Instant::now() >= deadline,
        )? {
            return Ok(state);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        harness.update(EXIT_STATUS_POLL_INTERVAL.min(remaining));
        let sleep_for =
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now()));
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    }
}

// ── grok wrap e2e ───────────────────────────────────────────────────────

/// `grok wrap` run budget.
/// Same contention math as the requirements-version test.
/// The child's cold exec of the huge debug binary can land its first write well past 30s under the parallel pty_e2e suite.
#[cfg(unix)]
pub(crate) const WRAP_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(unix)]
const WRAP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `grok wrap <wrap_args...>` to completion inside a PTY with an isolated `GROK_HOME`.
/// Returns the exit code (`None` only while still running at [`WRAP_TIMEOUT`]) and everything the wrap PTY emitted.
/// `extra_env` is where tests pin `SHELL`; wrap needs no mock content (it dispatches in `main` before auth/network/sandbox).
#[cfg(unix)]
pub(crate) fn run_wrap(wrap_args: &[&str], extra_env: &[(&str, &str)]) -> (Option<u32>, String) {
    run_wrap_driving(wrap_args, extra_env, |_| {})
}

/// Like [`run_wrap`], but hands the live harness to `drive` right after spawn.
/// A test can interact mid-run (wait for output, deliver signals to wrap itself) before the exit-and-drain phase.
#[cfg(unix)]
pub(crate) fn run_wrap_driving(
    wrap_args: &[&str],
    extra_env: &[(&str, &str)],
    drive: impl FnOnce(&mut PtyHarness),
) -> (Option<u32>, String) {
    let binary = pager_binary().expect("resolve pager binary");
    let home = tempfile::tempdir().expect("home tempdir");
    let home_str = home.path().to_str().expect("utf8 home").to_owned();

    let mut args = vec!["wrap"];
    args.extend_from_slice(wrap_args);
    let mut env: Vec<(&str, &str)> = vec![("GROK_HOME", &home_str), ("NO_COLOR", "1")];
    env.extend_from_slice(extra_env);

    let mut harness =
        PtyHarness::new_inherited_env(&binary, DEFAULT_ROWS, DEFAULT_COLS, &args, &env, None)
            .expect("spawn grok wrap");

    drive(&mut harness);

    let code = match wait_for_exit_status(&mut harness, WRAP_TIMEOUT) {
        Ok(PtyExitPoll::Exited(code)) => {
            harness.update(WRAP_DRAIN_TIMEOUT);
            Some(code)
        }
        Ok(PtyExitPoll::Running) => {
            harness.quit().expect("kill grok wrap after timeout");
            None
        }
        Ok(PtyExitPoll::PendingStatus) => {
            panic!("grok wrap exited but portable status remained unavailable for {WRAP_TIMEOUT:?}")
        }
        Err(error) => panic!("poll grok wrap exit: {error:#}"),
    };

    let raw = String::from_utf8_lossy(harness.raw_output()).into_owned();
    (code, raw)
}

/// Write an executable fake `$SHELL` that prints each argv element on its own `ARG:`-prefixed line and exits 0.
/// Tests can then assert the exact argv `grok wrap` hands to the user's shell without depending on any real shell's rc files or alias state.
/// Keep the returned tempdir alive for the duration of the run.
#[cfg(unix)]
pub(crate) fn fake_argv_echo_shell() -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("fake shell tempdir");
    let path = dir.path().join("fakeshell");
    std::fs::write(
        &path,
        "#!/bin/sh\nfor a in \"$@\"; do printf 'ARG:%s\\n' \"$a\"; done\n",
    )
    .expect("write fake shell");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake shell");
    let path_str = path.to_str().expect("utf8 fake shell path").to_owned();
    (dir, path_str)
}

// ── Shared polling / failure-dump / cast helpers ────────────────────────

/// Poll `probe` every 100ms until it yields `Some` or `timeout` elapses.
#[cfg(unix)]
pub(crate) fn poll_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Poll `cond` every 100ms until it returns true or `timeout` elapses.
#[cfg(unix)]
pub(crate) fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    poll_for(timeout, || cond().then_some(())).is_some()
}

/// Byte offset of the first `needle` in `haystack` at or after `from`; `None` past the end or when absent.
pub(crate) fn raw_position_after(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let tail = haystack.get(from..)?;
    tail.windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Escape sequences never render as text, so a byte-level wait is the only way to observe them.
pub(crate) fn wait_for_raw_bytes_after(
    harness: &mut PtyHarness,
    offset: usize,
    needle: &[u8],
    timeout: Duration,
) -> Option<usize> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(position) = raw_position_after(harness.raw_output(), offset, needle) {
            return Some(position);
        }
        if Instant::now() >= deadline {
            return None;
        }
        harness.update(Duration::from_millis(20));
    }
}

/// WezTerm is classified and probe-allowlisted; the harness scrubs the runner's own terminal markers before applying this.
const KITTY_CAPABLE_ENV: &[(&str, &str)] = &[("TERM_PROGRAM", "WezTerm")];

/// crossterm's `supports_keyboard_enhancement` probe: a flags query followed by DA1.
const KITTY_STARTUP_PROBE: &[u8] = b"\x1b[?u\x1b[c";

/// kitty's answer, trailing `;` included.
const KITTY_STARTUP_PROBE_REPLY: &[u8] = b"\x1b[?0u\x1b[?62;c";

/// Disambiguate (1) + report event types (2).
pub(crate) const KITTY_PUSH_FLAGS: &[u8] = b"\x1b[>3u";

/// crossterm pops one stack entry explicitly, not the bare `CSI < u`.
pub(crate) const KITTY_POP_FLAGS: &[u8] = b"\x1b[<1u";

pub(crate) const DA1_QUERY: &[u8] = b"\x1b[c";

pub(crate) const DA1_REPLY: &[u8] = b"\x1b[?62;c";

/// The probe blocks startup for at most 2 s, so the reply is scripted as soon as the probe appears, not after the welcome screen.
pub(crate) fn answer_kitty_startup_probe(harness: &mut PtyHarness) {
    let probe_at = wait_for_raw_bytes_after(harness, 0, KITTY_STARTUP_PROBE, WELCOME_TIMEOUT)
        .expect("pager never probed for kitty keyboard support");
    harness
        .inject_keys(KITTY_STARTUP_PROBE_REPLY)
        .expect("inject kitty probe reply");
    let pushed_at = wait_for_raw_bytes_after(harness, probe_at, KITTY_PUSH_FLAGS, WELCOME_TIMEOUT);
    assert!(
        pushed_at.is_some(),
        "pager did not push kitty keyboard flags after a supporting probe reply"
    );
}

/// Prints whatever the pager left in the tty input queue between two markers after it exits. `min 0 time 20` gives `cat`
/// EOF after two quiet seconds, `-echo` stops the tty echoing the residue a second time, and `LEFTOVER-BEGIN` follows a
/// successful `stty`, so its appearance means `cat` is about to read.
#[cfg(unix)]
pub(crate) const LEFTOVER_CAPTURE_SCRIPT: &str = concat!(
    "command -v stty >/dev/null || exit 99; ",
    "\"$0\" \"$@\"; rc=$?; ",
    "stty -icanon -echo min 0 time 20 || exit 98; ",
    "printf LEFTOVER-BEGIN; cat; ",
    "printf \"LEFTOVER-END EXIT=%s\" \"$rc\"",
);

#[cfg(unix)]
pub(crate) const LEFTOVER_BEGIN: &[u8] = b"LEFTOVER-BEGIN";

/// Waited for whole: a PTY read can split one `printf`.
#[cfg(unix)]
pub(crate) const LEFTOVER_END_OK: &[u8] = b"LEFTOVER-END EXIT=0";

/// Content-backed: the inherited-env spawn has no credentials on CI and stalls on the login screen.
#[cfg(unix)]
pub(crate) fn spawn_kitty_pager_with_leftover_capture(content: &ContentController) -> PtyHarness {
    let binary = pager_binary().expect("resolve pager binary");
    let pager = binary.to_str().expect("pager path is utf-8");
    let mut harness = PtyHarness::spawn_with_content_env(
        Path::new("/bin/sh"),
        DEFAULT_ROWS,
        DEFAULT_COLS,
        content,
        &["-c", LEFTOVER_CAPTURE_SCRIPT, pager],
        KITTY_CAPABLE_ENV,
    )
    .expect("spawn pager under the leftover-capture shell");
    answer_kitty_startup_probe(&mut harness);
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
}

/// The first Ctrl+C arms the quit confirmation, the second confirms. Returns the `raw_output()` length before the presses
/// so callers scan only the teardown suffix.
pub(crate) fn quit_with_double_ctrl_c(harness: &mut PtyHarness) -> usize {
    let pre = harness.raw_output().len();
    harness.inject_keys(keys::CTRL_C).expect("ctrl-c arm");
    harness.update(Duration::from_millis(250));
    harness.inject_keys(keys::CTRL_C).expect("ctrl-c confirm");
    pre
}

/// Dump assistant/tool/user messages (skipping the huge system prompt) from every request body.
/// Lets a failure message show tool args / tool results / user queries without printing megabytes of `{:#?}` bodies.
#[cfg(unix)]
pub(crate) fn dump_non_system_messages(bodies: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for (i, b) in bodies.iter().enumerate() {
        let msgs = b.get("messages").or_else(|| b.get("input"));
        let Some(arr) = msgs.and_then(|m| m.as_array()) else {
            continue;
        };
        for m in arr {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            if role == "system" {
                continue;
            }
            let s = serde_json::to_string(m).unwrap_or_default();
            out.push_str(&format!(
                "[req{i} {role}] {}\n",
                s.chars().take(900).collect::<String>()
            ));
        }
    }
    out
}

/// Extract the runtime task id from a serialized request body containing the background-start tool result's `<task-id>…</task-id>` envelope.
#[cfg(unix)]
pub(crate) fn extract_task_id(body: &str) -> Option<String> {
    let start = body.find("<task-id>")? + "<task-id>".len();
    let rest = &body[start..];
    let end = rest.find("</task-id>")?;
    let id = rest[..end].trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Dump an asciinema cast of `harness` into `$GROK_PTY_CAST_DIR/<file_name>` when the env var is set.
/// Failures are logged, never fatal: the cast is a diagnostic artifact, not part of what the test asserts.
#[cfg(unix)]
pub(crate) fn write_cast_if_requested(harness: &PtyHarness, file_name: &str) {
    let Ok(dir) = std::env::var("GROK_PTY_CAST_DIR") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let path = std::path::PathBuf::from(dir).join(file_name);
    match harness.write_cast(&path) {
        Ok(()) => eprintln!("wrote cast: {}", path.display()),
        Err(e) => eprintln!("failed to write cast {}: {e}", path.display()),
    }
}

/// Dump the current screen (plain text and HTML) into `$GROK_PTY_CAST_DIR/<file_stem>.{txt,html}` when the env var is set.
/// Failures are logged, never fatal; same opt-in as [`write_cast_if_requested`].
pub(crate) fn write_screen_dump_if_requested(harness: &PtyHarness, file_stem: &str) {
    let Ok(dir) = std::env::var("GROK_PTY_CAST_DIR") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let dir = std::path::PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("failed to create dump dir {}: {e}", dir.display());
        return;
    }
    for (ext, body) in [
        ("txt", harness.screen_contents()),
        ("html", harness.screen_html()),
    ] {
        let path = dir.join(format!("{file_stem}.{ext}"));
        match std::fs::write(&path, body) {
            Ok(()) => eprintln!("wrote screen dump: {}", path.display()),
            Err(e) => eprintln!("failed to write screen dump {}: {e}", path.display()),
        }
    }
}

// ── Clipboard paste e2e tests ───────────────────────────────────────────

/// Serialized `content` of every user message across recorded requests, in order. Like
/// [`all_user_messages`] but multimodal-tolerant: array-form content serializes to its JSON string
/// instead of vanishing.
pub(crate) fn all_user_message_blobs(content: &ContentController) -> Vec<String> {
    content
        .request_bodies()
        .iter()
        .flat_map(|b| {
            // Chat Completions carries `messages`; the Responses shape carries the same role/content layout under `input`
            let items = b["messages"].as_array().or_else(|| b["input"].as_array());
            items
                .into_iter()
                .flatten()
                .filter(|m| m["role"] == "user")
                .map(|m| match m["content"].as_str() {
                    Some(s) => s.to_owned(),
                    None => m["content"].to_string(),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

// The `paste_ctrl_v_*_{macos,windows}` tests drive the REAL host clipboard via the harness's shared `host_clipboard` helpers
// They are OS-native and mutate the machine-global clipboard (pbcopy/osascript on macOS, PowerShell on Windows)
// They hold `#[serial_test::serial(host_clipboard)]` so two clipboard tests never interleave within one test process
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) use distill_pager_pty_harness::host_clipboard::{
    HostClipboardTextGuard, pbcopy, set_clipboard_png, write_fixture_png,
};
// Windows CI sessions may lack a usable clipboard; the windows twins gate on this and SKIP instead of failing on environment
#[cfg(target_os = "windows")]
pub(crate) use distill_pager_pty_harness::host_clipboard::clipboard_roundtrip_works;

#[cfg(test)]
mod exit_status_wait_policy_tests {
    use super::*;

    #[test]
    fn waits_for_running_and_pending_until_deadline_and_propagates_errors() {
        assert_eq!(
            resolve_exit_status_poll::<u32, &'static str>(Ok(PtyExitPoll::Exited(2)), false),
            Ok(Some(PtyExitPoll::Exited(2)))
        );
        assert_eq!(
            resolve_exit_status_poll::<u32, &'static str>(Ok(PtyExitPoll::Running), false),
            Ok(None)
        );
        assert_eq!(
            resolve_exit_status_poll::<u32, &'static str>(Ok(PtyExitPoll::PendingStatus), false),
            Ok(None)
        );
        assert_eq!(
            resolve_exit_status_poll::<u32, &'static str>(Ok(PtyExitPoll::Running), true),
            Ok(Some(PtyExitPoll::Running))
        );
        assert_eq!(
            resolve_exit_status_poll::<u32, &'static str>(Ok(PtyExitPoll::PendingStatus), true),
            Ok(Some(PtyExitPoll::PendingStatus))
        );
        assert_eq!(
            resolve_exit_status_poll::<u32, &'static str>(Err("poll failed"), true),
            Err("poll failed")
        );
    }
}
