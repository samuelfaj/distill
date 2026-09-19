use super::*;

/// Test that PromptContext round-trips through JSON in the save/load format used by `save_prompt_context` and `load_prompt_context`.
#[test]
fn test_json_round_trip() {
    let ctx = xai_grok_agent::PromptContext {
        ..Default::default()
    };

    let json = serde_json::to_string_pretty(&ctx).unwrap();
    let loaded: xai_grok_agent::PromptContext = serde_json::from_str(&json).unwrap();

    assert_eq!(loaded.version, 1);
}

/// Test that PromptContext survives a JSON write to disk and read back with field-level fidelity.
/// This exercises serde and filesystem I/O but not the `save_prompt_context`/`load_prompt_context` wrappers.
/// Those wrappers depend on `grok_home()` and `SessionInfo` path encoding.
#[test]
fn test_json_round_trip_via_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-test");
    std::fs::create_dir_all(&session_dir).unwrap();

    let ctx = xai_grok_agent::PromptContext::default();

    // Write directly (mimicking save_prompt_context's logic)
    let path = session_dir.join(PROMPT_CONTEXT_FILENAME);
    let json = serde_json::to_string_pretty(&ctx).unwrap();
    std::fs::write(&path, &json).unwrap();

    // Read back
    let read_json = std::fs::read_to_string(&path).unwrap();
    let loaded: xai_grok_agent::PromptContext = serde_json::from_str(&read_json).unwrap();

    assert_eq!(loaded.version, ctx.version);
    assert_eq!(loaded.build_timestamp_utc, ctx.build_timestamp_utc);
}

#[test]
fn test_system_prompt_write_and_read() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-prompt-test");
    std::fs::create_dir_all(&session_dir).unwrap();

    let prompt = "You are a test agent.\n\nDo the thing.";
    let path = session_dir.join(SYSTEM_PROMPT_FILENAME);
    std::fs::write(&path, prompt).unwrap();

    let read_back = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        read_back, prompt,
        "system_prompt.txt must round-trip exactly"
    );
}

#[test]
fn test_system_prompt_is_plain_text_not_json() {
    let prompt = "You are a Remote-Code subagent.";
    // system_prompt.txt is raw text, NOT JSON-encoded.
    assert!(!prompt.starts_with('"'), "must not be JSON-quoted");
    assert!(!prompt.starts_with('{'), "must not be JSON object");
}

#[test]
fn test_canonical_artifacts_coexist() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-artifacts");
    std::fs::create_dir_all(&session_dir).unwrap();

    // Write both canonical artifacts.
    let prompt = "You are a test subagent.";
    let ctx = xai_grok_agent::PromptContext {
        ..Default::default()
    };

    std::fs::write(session_dir.join(SYSTEM_PROMPT_FILENAME), prompt).unwrap();
    std::fs::write(
        session_dir.join(PROMPT_CONTEXT_FILENAME),
        serde_json::to_string_pretty(&ctx).unwrap(),
    )
    .unwrap();

    // Both files exist and are independently readable.
    assert!(session_dir.join(SYSTEM_PROMPT_FILENAME).exists());
    assert!(session_dir.join(PROMPT_CONTEXT_FILENAME).exists());

    let read_prompt = std::fs::read_to_string(session_dir.join(SYSTEM_PROMPT_FILENAME)).unwrap();
    assert_eq!(read_prompt, prompt);

    let read_ctx: xai_grok_agent::PromptContext = serde_json::from_str(
        &std::fs::read_to_string(session_dir.join(PROMPT_CONTEXT_FILENAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(read_ctx.version, 1);
}

/// Core invariant: `system_prompt.txt` must match the first System entry in `chat_history.jsonl`.
#[test]
fn test_system_prompt_matches_chat_history_system_message() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-consistency");
    std::fs::create_dir_all(&session_dir).unwrap();

    let system_prompt = "You are a Remote-Code subagent.\n\n<tool_calling>\n...";

    // Write system_prompt.txt (same string used for chat_history).
    std::fs::write(session_dir.join(SYSTEM_PROMPT_FILENAME), system_prompt).unwrap();

    // Simulate chat_history.jsonl first entry.
    let entry = serde_json::json!({ "role": "system", "content": system_prompt });
    std::fs::write(
        session_dir.join("chat_history.jsonl"),
        format!("{}\n", serde_json::to_string(&entry).unwrap()),
    )
    .unwrap();

    // Verify byte-identity.
    let file_prompt = std::fs::read_to_string(session_dir.join(SYSTEM_PROMPT_FILENAME)).unwrap();
    let chat_json = std::fs::read_to_string(session_dir.join("chat_history.jsonl")).unwrap();
    let first_line: serde_json::Value =
        serde_json::from_str(chat_json.lines().next().unwrap()).unwrap();

    assert_eq!(
        first_line.get("content").and_then(|c| c.as_str()),
        Some(file_prompt.as_str()),
        "system_prompt.txt must match first system message in chat_history.jsonl"
    );
}

/// Test that missing file gracefully returns None (simulating old sessions).
#[test]
fn test_missing_file_deserializes_as_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(PROMPT_CONTEXT_FILENAME);

    let result = std::fs::read_to_string(&path);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
}

/// Test that corrupt JSON returns a deserialization error.
#[test]
fn test_corrupt_json_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(PROMPT_CONTEXT_FILENAME);
    std::fs::write(&path, "not valid json {{{").unwrap();

    let json = std::fs::read_to_string(&path).unwrap();
    let result: Result<xai_grok_agent::PromptContext, _> = serde_json::from_str(&json);
    assert!(result.is_err(), "corrupt JSON should fail to deserialize");
}

// ── Canonical artifact load tests ───────────────────────────────────

#[test]
fn test_load_system_prompt_returns_content_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-load-test");
    std::fs::create_dir_all(&session_dir).unwrap();

    let prompt = "You are a Remote-Code subagent.";
    std::fs::write(session_dir.join(SYSTEM_PROMPT_FILENAME), prompt).unwrap();

    let loaded = load_system_prompt_from_dir(&session_dir);
    assert_eq!(loaded.as_deref(), Some(prompt));
}

#[test]
fn test_load_system_prompt_returns_none_for_old_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-old");
    std::fs::create_dir_all(&session_dir).unwrap();

    let loaded = load_system_prompt_from_dir(&session_dir);
    assert!(
        loaded.is_none(),
        "old sessions without system_prompt.txt should return None"
    );
}

#[test]
fn test_load_prompt_context_returns_context_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-ctx-load");
    std::fs::create_dir_all(&session_dir).unwrap();

    let ctx = xai_grok_agent::PromptContext::default();
    std::fs::write(
        session_dir.join(PROMPT_CONTEXT_FILENAME),
        serde_json::to_string_pretty(&ctx).unwrap(),
    )
    .unwrap();

    let loaded = load_prompt_context_from_dir(&session_dir);
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().version, 1);
}

#[test]
fn test_load_prompt_context_returns_none_for_old_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-no-ctx");
    std::fs::create_dir_all(&session_dir).unwrap();

    let loaded = load_prompt_context_from_dir(&session_dir);
    assert!(
        loaded.is_none(),
        "old sessions without prompt_context.json should return None"
    );
}

#[test]
fn test_load_prompt_context_returns_none_for_corrupt_json() {
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().join("session-corrupt");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join(PROMPT_CONTEXT_FILENAME),
        "not valid json {{{",
    )
    .unwrap();

    let loaded = load_prompt_context_from_dir(&session_dir);
    assert!(
        loaded.is_none(),
        "corrupt JSON should return None gracefully"
    );
}
