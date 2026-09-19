// Modified for Distill by Samuel Fajreldines, 2026.
//! Tool dispatch helpers for `SessionActor`.
//! Covers `dispatch_tool` and its lock and display helpers, direct bash-mode execution, and tool argument parse-error formatting.

use super::*;
use std::path::PathBuf;

/// Number of output lines to show in final bash mode output summary
const BASH_MODE_FINAL_OUTPUT_LINES: usize = 10;
const BASH_MODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Phase 2: dispatch a tool call through [`WorkspaceOps::call_tool`].
///
/// Agent sessions always use local workspace ops (in-process toolset).
pub(super) async fn dispatch_tool(
    workspace_ops: &distill_workspace::WorkspaceOps,
    prepared: &PreparedToolCall,
    session_id: &str,
) -> Result<ToolRunResult, distill_tool_runtime::ToolError> {
    tracing::debug!(
        tool = %prepared.tool_name,
        call_id = %prepared.tool_call_id.0,
        session = %session_id,
        mode = "local",
        "dispatch_tool"
    );
    workspace_ops
        .call_tool(
            &prepared.tool_name,
            prepared.parsed_args.clone(),
            &prepared.tool_call_id.0,
            Some(session_id),
        )
        .await
}

/// First string-valued argument among `keys`, in priority order.
fn str_arg<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k)?.as_str())
}

/// Extract the workspace path that a tool call targets, to serialize concurrent same-file edits inside `execute_tool_calls`.
/// `file_path`: distill (`search_replace`), opencode (`EditTool`, `WriteTool`, `ReadTool`), codex (`read_file`).
/// `target_directory` is deliberately omitted: a directory listing isn't an edit and must not share a file lock.
pub(super) fn lock_path_for_args(args: &serde_json::Value, cwd: &Path) -> Option<String> {
    let input = Path::new(str_arg(args, &["file_path", "path", "target_file"])?);
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    let lock_path = canonicalize_existing_ancestor(&normalized).unwrap_or(normalized);
    Some(lock_path.to_string_lossy().into_owned())
}

fn canonicalize_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = dunce::canonicalize(ancestor) {
            suffix.reverse();
            canonical.extend(suffix);
            return Some(canonical);
        }
        suffix.push(ancestor.file_name()?.to_owned());
        ancestor = ancestor.parent()?;
    }
}

/// Pull the path a read/list tool targets and classify it against the store.
/// Keys span harnesses: `read_file` uses `target_file`, grep uses `path`, `list_dir` uses `target_directory`.
/// The path grammar lives in `distill_compaction_transcript`.
pub(super) fn compaction_artifact_read(
    args: &serde_json::Value,
) -> Option<distill_compaction_transcript::CompactionArtifact> {
    let path = str_arg(
        args,
        &["target_file", "file_path", "path", "target_directory"],
    )?;
    distill_compaction_transcript::classify_compaction_path(path)
}

/// Map a backend-hosted tool name to a user-facing title, ACP ToolKind, and `raw_input` JSON for display in the pager's tool call UI.
/// The `raw_input` carries metadata that the pager's `tool_call_to_block()` uses to select the correct renderer.
pub(super) fn backend_tool_display(name: &str) -> (String, acp::ToolKind, serde_json::Value) {
    match name {
        "web_search" => (
            "Web search:".to_string(),
            acp::ToolKind::Search,
            serde_json::json!({"variant": "WebSearch", "backend": true}),
        ),
        "x_search" => (
            "X search:".to_string(),
            acp::ToolKind::Search,
            serde_json::json!({"variant": "XSearch", "backend": true}),
        ),
        n => (
            n.to_string(),
            acp::ToolKind::Other,
            serde_json::json!({"backend": true}),
        ),
    }
}

/// The backend reports each call's real success or failure in the payload's `status` field.
/// A `"failed"` status becomes [`acp::ToolCallStatus::Failed`]; any other or absent status stays `Completed`.
/// Consumers, notably the headless `streaming-messages-json` `web_search_tool_result_error` branch, see the real failure instead of `Completed`.
pub(super) fn backend_tool_call_status(result: Option<&serde_json::Value>) -> acp::ToolCallStatus {
    let failed = result
        .and_then(|r| r.get("status"))
        .and_then(serde_json::Value::as_str)
        == Some("failed");
    if failed {
        acp::ToolCallStatus::Failed
    } else {
        acp::ToolCallStatus::Completed
    }
}

/// Expose the resolved model ID only when the backend actually routed elsewhere AND the catalog opted this model into checkpoint identity.
/// It checks the same `show_model_fingerprint` flag as the fingerprint itself, so one server-side setting governs both.
/// The client keeps no per-slug default.
pub(super) fn should_show_resolved_model(
    requested: &str,
    resolved: &str,
    show_checkpoint_identity: bool,
) -> bool {
    show_checkpoint_identity && requested != resolved
}

/// Resolve the shell name for the system prompt `Shell:` field.
/// Unix: basename of `$SHELL`.
/// Windows: name from the `detect_windows_shell` cascade (pwsh, then powershell.exe, then Git Bash, then cmd.exe), since `$SHELL` is absent.
pub(super) fn resolve_session_shell() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                std::path::Path::new(&s)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "bash".to_string())
    }

    #[cfg(not(unix))]
    {
        distill_config::shell::detect_windows_shell()
            .name()
            .to_string()
    }
}

/// Key in `ToolError::details` that carries the HTTP status code.
/// Used by both error producers (image_gen, video_gen, test helpers) and the `is_auth_tool_error` classifier to avoid accidental key mismatch.
pub(crate) const HTTP_STATUS_DETAILS_KEY: &str = "status";

impl SessionActor {
    pub(super) fn extract_bash_command(prompt_blocks: &[acp::ContentBlock]) -> Option<String> {
        crate::extensions::prompt_meta::PromptBlockMeta::command_in(prompt_blocks)
    }

    /// Handle a direct bash command from bash mode.
    /// Runs the command with streaming output and sends updates to the TUI.
    pub(super) async fn handle_direct_bash_command(
        &self,
        _prompt_id: &str,
        command: String,
        prompt_blocks: &[acp::ContentBlock],
    ) -> PromptTurnResult {
        tracing::info!("Handling direct bash command");

        // Send user message chunks to scrollback (so the user sees their command)
        let model_id = self.current_model_id().await;
        let user_chunk_meta = serde_json::json!({ "modelId": model_id })
            .as_object()
            .cloned();
        for block in prompt_blocks.iter() {
            let update = acp::SessionUpdate::UserMessageChunk(
                acp::ContentChunk::new(block.clone()).meta(user_chunk_meta.clone()),
            );
            let notification_meta = self.build_notification_meta();
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::Update(SessionUpdate::Acp(Box::new(
                    acp::SessionNotification::new(self.session_info.id.clone(), update)
                        .meta(notification_meta.as_object().cloned()),
                ))));
        }

        // Persist the user message for session history
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
                prompt_blocks.to_vec(),
            )));
        // Bash turns bypass `handle_prompt`'s commit point; the command is now in the ordered persistence stream, so a send-now may cancel this turn
        self.mark_front_message_committed().await;

        // Run the bash command with streaming enabled
        let tool_call_id = acp::ToolCallId::from(format!("bash-mode-{}", uuid::Uuid::new_v4()));

        // Send initial ToolCall to register with TUI

        use distill_tools::types::ToolInput;
        // Use the stripped command as the description so the pager shows the real command (not a generic label) while satisfying the required field
        let title_command = distill_tools::util::strip_redundant_session_cd(
            &command,
            self.tool_context.cwd.as_path(),
        );
        let tool_input = ToolInput::Bash(BashToolInput {
            command: command.clone(),
            timeout: None,
            description: title_command.clone().into_owned(),
            is_background: false,
        });
        // Bash mode has no model-issued wire name; resolve the toolset's execute tool by kind so the x.ai/tool identity still stamps
        let bash_marker = serde_json::json!({"bash_mode": true}).as_object().cloned();
        let exec_wire = {
            let agent = self.agent.borrow();
            agent
                .tool_bridge()
                .toolset()
                .tool_name_for_kind(distill_tools::types::tool::ToolKind::Execute)
        };
        let bash_meta = match exec_wire {
            Some(wire) => self.stamp_tool_meta(bash_marker.clone(), &wire, Some(&tool_input)),
            None => bash_marker,
        };
        self.send_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(tool_call_id.clone(), format!("Execute `{title_command}`"))
                    .kind(acp::ToolKind::Execute)
                    .status(acp::ToolCallStatus::InProgress)
                    .content(Vec::new())
                    .locations(Vec::new())
                    .raw_input(serde_json::to_value(&tool_input).ok())
                    .meta(bash_meta),
            ),
            None,
        )
        .await;

        let request = TerminalRunRequest {
            tool_call_id: tool_call_id.clone(),
            command: command.clone(),
            cwd: self.tool_context.cwd.clone(),
            env: self.tool_context.session_env.as_ref().clone(),
            timeout: BASH_MODE_TIMEOUT,
            output_byte_limit: 1_048_576, // 1 MiB
            stream: true,                 // Enable streaming for bash mode
            output_file: None,            // No file logging for interactive bash mode
        };

        let result = self.tool_context.terminal.run(request).await;

        // Format the output
        let (output, exit_code, timed_out, signal) = match result {
            Ok(res) => (
                res.combined_output,
                res.exit_code.unwrap_or(-1),
                res.timed_out,
                res.signal,
            ),
            Err(e) => (format!("Error running command: {}", e), -1, false, None),
        };

        // Full stdout for the TUI; prompt/history keep a last-N tail so dumps do not inflate the next turn
        let full_output = output.trim_end().to_string();
        let lines: Vec<&str> = full_output.lines().collect();
        let total_lines = lines.len();
        let history_output = if total_lines > BASH_MODE_FINAL_OUTPUT_LINES {
            let start = total_lines - BASH_MODE_FINAL_OUTPUT_LINES;
            let last_lines = lines.get(start..).unwrap_or(&[]).join("\n");
            format!("... ({} lines)\n{}", total_lines, last_lines)
        } else {
            full_output.clone()
        };

        let is_backgrounded = signal.as_deref() == Some("backgrounded");

        // Send final tool call update
        // For backgrounded commands, don't mark as completed/failed; let the background task do that
        if !is_backgrounded {
            let final_status = if exit_code == 0 && signal.is_none() {
                acp::ToolCallStatus::Completed
            } else {
                acp::ToolCallStatus::Failed
            };
            let bash_output = BashOutput {
                output_for_prompt: BashOutput::make_output_for_prompt(&history_output),
                output: full_output.as_bytes().to_vec(),
                exit_code,
                command: command.clone(),
                truncated: false,
                signal: signal.clone(),
                timed_out,
                description: None,
                current_dir: self.tool_context.cwd.to_string(),
                output_file: String::new(),
                total_bytes: full_output.len(),
                output_delta: None,
                was_bare_echo: false,
            };
            self.send_update(
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    tool_call_id,
                    acp::ToolCallUpdateFields::new()
                        .status(Some(final_status))
                        .raw_output(serde_json::to_value(ToolsToolOutput::Bash(bash_output)).ok()),
                )),
                None,
            )
            .await;
        }

        // No AgentMessageChunk summary is sent here: the execute block already shows the full output, so an agent copy would duplicate scrollback
        // Old sessions that persisted one still replay fine

        // Build a single user message for chat history that includes command, output, and exit code
        let user_message = format!(
            "I executed a terminal command: `{}`\n\nOutput:\n```\n{}\n```\n\n[exit code: {}]",
            command, history_output, exit_code
        );

        // Add to chat history as a user message only
        self.chat_state_handle
            .push_user_message(ConversationItem::user(&user_message));

        self.chat_state_handle.flush();

        let flush_error = self.flush_to_disk().await.err();
        self.disk_full_acp_error(flush_error.as_ref())?;

        let total_tokens = self.chat_state_handle.get_total_tokens().await;
        ok_end_turn(total_tokens, None)
    }
}

// ── Tool argument error formatting ─────────────────────────────────────

// `truncate_bytes` is the UTF-8-safe truncation helper from distill-sampling-types

/// Maximum bytes of `raw_arguments` echoed in a parse-error tool_result.
/// The model already holds the full arguments in context, so a prefix plus the JSON error position is enough; echoing more grows every later turn.
/// A syntax error position past this limit points into truncated text, but the model still has the full arguments in context.
pub(crate) const MAX_ARGS_IN_ERROR: usize = 2_000;

/// Build the user-facing error message shown when tool arguments cannot be parsed.
/// The message is stored as a `tool_result` in the conversation history, so the model sees it on the very next turn.
/// Without the echoed original, the model would only see that empty object and have to regenerate all its work from scratch.
pub(super) fn build_tool_parse_error_message(
    function_name: &str,
    err: &distill_tool_runtime::ToolError,
    raw_arguments: &str,
) -> String {
    let mut msg = format!("Failed to parse arguments for tool `{function_name}`: {err}");

    if raw_arguments.is_empty() {
        return msg;
    }

    // Append the original arguments (capped) so the model knows what it sent.
    // Use truncate_bytes to avoid panicking on a multi-byte UTF-8 boundary.
    msg.push_str("\n\nYour original arguments:\n");
    let prefix = truncate_bytes(raw_arguments, MAX_ARGS_IN_ERROR);
    msg.push_str(prefix);
    if prefix.len() < raw_arguments.len() {
        msg.push_str("\n... (truncated)");
    }

    // If the arguments string is not valid JSON, append the exact position of the syntax error so the model can fix it directly
    // Use `IgnoredAny`: we only need the error, not a DOM
    if let Err(json_err) = serde_json::from_str::<serde::de::IgnoredAny>(raw_arguments) {
        msg.push_str(&format!(
            "\n\nNote: the arguments above contain invalid JSON — {json_err}\n\
             Please fix the syntax and retry."
        ));
    }

    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_sampling_types::rs;

    fn web_search_payload(status: rs::WebSearchToolCallStatus) -> serde_json::Value {
        // The exact serialized `web_search_call` payload the sampler forwards on `BackendToolCallCompleted` (via `serde_json::to_value(ws)`)
        serde_json::to_value(rs::WebSearchToolCall {
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "rust async runtime".to_string(),
                sources: None,
            }),
            id: "ws1".to_string(),
            status,
        })
        .expect("serialize web_search_call payload")
    }

    /// A backend web-search failure must map to ACP `Failed` so the headless `web_search_tool_result_error` branch is reachable in production.
    /// A completed call or an absent payload stays `Completed`.
    /// Exercises the real payload shape, not a hand-built status.
    #[test]
    fn backend_failed_web_search_maps_to_failed_status() {
        let failed = web_search_payload(rs::WebSearchToolCallStatus::Failed);
        assert_eq!(
            failed
                .pointer("/status")
                .unwrap_or(&serde_json::Value::Null),
            "failed",
            "wire field name is `status`"
        );
        assert_eq!(
            backend_tool_call_status(Some(&failed)),
            acp::ToolCallStatus::Failed
        );

        let completed = web_search_payload(rs::WebSearchToolCallStatus::Completed);
        assert_eq!(
            backend_tool_call_status(Some(&completed)),
            acp::ToolCallStatus::Completed
        );

        assert_eq!(
            backend_tool_call_status(None),
            acp::ToolCallStatus::Completed
        );
    }
}
