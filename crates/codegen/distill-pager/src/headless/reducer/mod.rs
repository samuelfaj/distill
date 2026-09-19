// Modified for Distill by Samuel Fajreldines, 2026.
//! Streaming reducers that fold the agent ACP event stream into NDJSON lines.
//! Each wire format has one `Reducer` impl, selected by [`reducer_for`].
//! This module owns the shared [`StreamEvent`]/[`Reducer`] types; each format is a submodule.

use agent_client_protocol as proto;
use serde::Serialize;
use serde_json::{Value, json};

use crate::headless::OutputFormat;

mod acp;
mod messages;

use self::acp::AcpReducer;
use self::messages::MessagesReducer;
use distill_shell::extensions::notification::ResponseUsage;

/// Serialize a wire line, degrading to an `error` line rather than panicking.
pub(crate) fn to_line<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value)
        .unwrap_or_else(|e| json!({"type": "error", "message": format!("serialize failed: {e}")}))
}

/// Merge the camelCase `structuredOutput`/`structuredOutputError` fields into a terminal wire line.
/// `Ok` stamps the value, `Err` stamps null plus the error.
pub(crate) fn attach_structured_output(
    target: &mut Value,
    structured: Option<Result<Value, String>>,
) {
    let Some(structured) = structured else { return };
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    let Some(obj) = target.as_object_mut() else {
        return;
    };
    match structured {
        Ok(value) => {
            obj.insert("structuredOutput".into(), value);
        }
        Err(err) => {
            obj.insert("structuredOutput".into(), Value::Null);
            obj.insert("structuredOutputError".into(), err.into());
        }
    }
}

/// One event from the agent stream, independent of transport; the single input every reducer folds.
pub(crate) enum StreamEvent {
    AgentMessage(String),
    AgentThought(String),
    ToolCall(ToolCallEvent),
    ToolCallUpdate(ToolCallUpdateEvent),
    Plan(Value),
    /// Session metadata; `skills` is the subset of `commands` that are skills.
    AvailableCommands {
        tools: Vec<String>,
        commands: Vec<String>,
        skills: Vec<String>,
    },
    Lifecycle(Lifecycle),
    /// One model response opened (Messages backend); it carries the real id, model, and input-side token counts.
    ResponseStarted {
        message_id: Option<String>,
        model: Option<String>,
        input_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
    },
    /// One reasoning block finished (Messages backend); it carries its signature for the in-order `signature_delta`.
    ReasoningCompleted {
        signature: Option<String>,
    },
    /// One model response finished; it carries its stop reason, id, usage, signature, and stop sequence.
    ResponseCompleted {
        message_id: Option<String>,
        stop_reason: Option<String>,
        usage: Option<ResponseUsage>,
        signature: Option<String>,
        /// Provider's matched stop sequence; set only by the Messages reducer.
        stop_sequence: Option<String>,
    },
}

pub(crate) struct ToolCallEvent {
    tool_call_id: String,
    title: String,
    tool_kind: Option<String>,
    status: Option<proto::ToolCallStatus>,
    tool_name: String,
    raw_input: Value,
    content: Value,
    locations: Value,
    /// True for Grok's backend `web_search`; it is folded inline instead of taking the split path client tools take.
    backend_web_search: bool,
}

pub(crate) struct ToolCallUpdateEvent {
    tool_call_id: String,
    status: Option<proto::ToolCallStatus>,
    content: Value,
    raw_output: Value,
    locations: Value,
}

pub(crate) enum Lifecycle {
    CompactStarted {
        percentage: u8,
    },
    CompactCompleted {
        pre_tokens: u64,
    },
    CompactFailed {
        error: String,
    },
    CompactCancelled,
    AutoContinue {
        total_tokens: u64,
    },
    ImageCompressed {
        message: String,
    },
    MemoryFlushStarted,
    MemoryFlushCompleted {
        result: String,
        path: Option<String>,
    },
    MemoryCaptureActivity {
        activity: String,
        from_turn: u32,
        through_turn: u32,
        attempt: u32,
        detail: Option<String>,
    },
}

impl Lifecycle {
    /// The human-readable `plain` line for this lifecycle event.
    pub(crate) fn plain_message(&self) -> String {
        match self {
            Lifecycle::CompactStarted { percentage } => {
                format!("Auto-compacting conversation ({percentage}% full)...")
            }
            Lifecycle::CompactCompleted { .. } => "Conversation compacted.".to_string(),
            Lifecycle::CompactFailed { error } if error.trim().is_empty() => {
                "Auto-compact failed.".to_string()
            }
            Lifecycle::CompactFailed { error } => {
                // Plain output is line-oriented, so the two-line error message collapses to one; JSON carries the raw string
                // The hyphen matches the TUI
                let single_line = error.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("Auto-compact failed - {single_line}")
            }
            Lifecycle::CompactCancelled => "Auto-compact cancelled.".to_string(),
            Lifecycle::AutoContinue { .. } => "Resumed after compaction.".to_string(),
            Lifecycle::ImageCompressed { message } => message.clone(),
            Lifecycle::MemoryFlushStarted => "Memory flush started.".to_string(),
            // `result`, `path`, and capture `detail` remain available in the
            // structured lifecycle event. Plain lines are trusted UI copy and
            // must not promote model/parser errors or local paths into it.
            Lifecycle::MemoryFlushCompleted { .. } => "Memory flush completed.".to_string(),
            Lifecycle::MemoryCaptureActivity {
                activity,
                from_turn,
                through_turn,
                attempt,
                detail: _,
            } => {
                let activity = match activity.as_str() {
                    "queued" => "queued",
                    "running" => "running",
                    "completed" => "completed",
                    "no_op" => "completed with no changes",
                    "retry" => "scheduled for retry",
                    "failed" => "failed",
                    _ => "updated",
                };
                let attempt_suffix = if *attempt > 1 {
                    format!(" (attempt {attempt})")
                } else {
                    String::new()
                };
                format!(
                    "Memory capture {activity} for turns {from_turn}-{through_turn}{attempt_suffix}."
                )
            }
        }
    }
}

/// The `streaming-json` wire token for an ACP [`proto::ToolKind`].
/// The match is typed rather than serde so renaming a variant is a compile error, not a dropped `None`.
pub(crate) fn tool_kind_wire(kind: proto::ToolKind) -> Option<String> {
    let token = match kind {
        proto::ToolKind::Read => "read",
        proto::ToolKind::Edit => "edit",
        proto::ToolKind::Delete => "delete",
        proto::ToolKind::Move => "move",
        proto::ToolKind::Search => "search",
        proto::ToolKind::Execute => "execute",
        proto::ToolKind::Think => "think",
        proto::ToolKind::Fetch => "fetch",
        proto::ToolKind::SwitchMode => "switch_mode",
        proto::ToolKind::Other => "other",
        _ => return serde_wire_token(&kind),
    };
    Some(token.to_string())
}

/// The `streaming-json` wire token for an ACP [`proto::ToolCallStatus`] (see [`tool_kind_wire`]).
pub(crate) fn tool_call_status_wire(status: proto::ToolCallStatus) -> Option<String> {
    let token = match status {
        proto::ToolCallStatus::Pending => "pending",
        proto::ToolCallStatus::InProgress => "in_progress",
        proto::ToolCallStatus::Completed => "completed",
        proto::ToolCallStatus::Failed => "failed",
        _ => return serde_wire_token(&status),
    };
    Some(token.to_string())
}

/// Fallback wire token for a future `#[non_exhaustive]` ACP variant not covered above.
fn serde_wire_token<T: serde::Serialize>(value: &T) -> Option<String> {
    match serde_json::to_value(value) {
        Ok(Value::String(s)) => Some(s),
        _ => None,
    }
}

/// Serialize an ACP value expected to be a JSON array, degrading to `[]` on failure.
fn json_array_or_empty<T: serde::Serialize>(value: &T) -> Value {
    match serde_json::to_value(value) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "headless: failed to serialize ACP value; emitting []");
            json!([])
        }
    }
}

/// Canonical model-facing tool name from the `x.ai/tool` `_meta` envelope, else the display title, else kind, else `"tool"`.
/// The shell stamps the wire name (`bash`, `x_search`, `read_file`) under `x.ai/tool.name`; the human title (`Execute ...`, `X search:`) is the fallback.
fn tool_name_from(meta: Option<&proto::Meta>, title: &str, kind: Option<&str>) -> String {
    if let Some(meta) = meta
        && let Some(name) = meta
            .get("x.ai/tool")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
        && !name.is_empty()
    {
        return name.to_string();
    }
    if !title.is_empty() {
        return title.to_string();
    }
    if let Some(kind) = kind
        && !kind.is_empty()
    {
        return kind.to_string();
    }
    "tool".to_string()
}

/// True iff `tc` is Grok's backend `web_search` (`_meta.backend` and `raw_input.variant == "WebSearch"`).
fn is_backend_web_search(meta: Option<&proto::Meta>, raw_input: &Value) -> bool {
    let backend = meta
        .and_then(|m| m.get("backend"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_web_search = raw_input.get("variant").and_then(Value::as_str) == Some("WebSearch");
    backend && is_web_search
}

/// Canonical tool kind from the `x.ai/tool` `_meta` envelope, else the ACP `ToolCall.kind`.
/// Client tools register as `ToolKind::Other` on the early notification, so the real kind (`read`, `edit`, `execute`) arrives in `x.ai/tool.kind`.
fn tool_kind_from(meta: Option<&proto::Meta>, kind: proto::ToolKind) -> Option<String> {
    if let Some(meta) = meta
        && let Some(k) = meta
            .get("x.ai/tool")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str())
        && !k.is_empty()
    {
        return Some(k.to_string());
    }
    tool_kind_wire(kind)
}

pub(crate) fn tool_call_event(tc: &proto::ToolCall) -> ToolCallEvent {
    let tool_kind = tool_kind_from(tc.meta.as_ref(), tc.kind);
    let tool_name = tool_name_from(tc.meta.as_ref(), &tc.title, tool_kind.as_deref());
    let raw_input = tc.raw_input.clone().unwrap_or(Value::Null);
    let backend_web_search = is_backend_web_search(tc.meta.as_ref(), &raw_input);
    ToolCallEvent {
        tool_call_id: tc.tool_call_id.0.to_string(),
        title: tc.title.clone(),
        tool_kind,
        status: Some(tc.status),
        tool_name,
        raw_input,
        content: json_array_or_empty(&tc.content),
        locations: json_array_or_empty(&tc.locations),
        backend_web_search,
    }
}

fn tool_call_update_event(tcu: &proto::ToolCallUpdate) -> ToolCallUpdateEvent {
    ToolCallUpdateEvent {
        tool_call_id: tcu.tool_call_id.0.to_string(),
        status: tcu.fields.status,
        content: tcu
            .fields
            .content
            .as_ref()
            .map_or_else(|| json!([]), json_array_or_empty),
        raw_output: tcu.fields.raw_output.clone().unwrap_or(Value::Null),
        locations: tcu
            .fields
            .locations
            .as_ref()
            .map_or_else(|| json!([]), json_array_or_empty),
    }
}

fn tools_from_meta(meta: Option<&proto::Meta>) -> Vec<String> {
    meta.and_then(|m| m.get("tools"))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn command_names(commands: &[proto::AvailableCommand]) -> Vec<String> {
    commands.iter().map(|c| c.name.clone()).collect()
}

/// Skill names from the `AvailableCommandsUpdate`: commands stamped with `_meta.scope` and `_meta.path`.
pub(crate) fn skill_names(commands: &[proto::AvailableCommand]) -> Vec<String> {
    commands
        .iter()
        .filter(|c| {
            c.meta
                .as_ref()
                .is_some_and(|m| m.get("scope").is_some() && m.get("path").is_some())
        })
        .map(|c| c.name.clone())
        .collect()
}

/// Map an ACP `SessionUpdate` to a [`StreamEvent`], or `None` for variants the reducers ignore.
pub(crate) fn map_session_update(update: &proto::SessionUpdate) -> Option<StreamEvent> {
    Some(match update {
        proto::SessionUpdate::ToolCall(tc) => StreamEvent::ToolCall(tool_call_event(tc)),
        proto::SessionUpdate::ToolCallUpdate(tcu) => {
            StreamEvent::ToolCallUpdate(tool_call_update_event(tcu))
        }
        proto::SessionUpdate::Plan(p) => StreamEvent::Plan(json_array_or_empty(&p.entries)),
        proto::SessionUpdate::AvailableCommandsUpdate(u) => StreamEvent::AvailableCommands {
            tools: tools_from_meta(u.meta.as_ref()),
            commands: command_names(&u.available_commands),
            skills: skill_names(&u.available_commands),
        },
        _ => return None,
    })
}

/// Session facts known once the ACP session is open.
pub(crate) struct SessionContext {
    pub session_id: String,
    pub model: Option<String>,
    pub cwd: String,
    pub permission_mode: Option<String>,
    pub mcp_servers: Vec<McpServer>,
    pub include_partial_messages: bool,
    /// True when the session authenticated with an API key (vs OAuth).
    pub api_key_auth: bool,
    /// The current model's total context window in tokens, when known.
    pub context_window: Option<u64>,
}

pub(crate) struct TurnEnd<'a> {
    pub stop_reason: &'a str,
    pub session_id: &'a str,
    pub request_id: &'a str,
    pub usage: Option<&'a Value>,
    pub structured_output: Option<Result<Value, String>>,
    pub result_text: &'a str,
    /// Total wall-clock time for the run (`duration_ms` on the Messages `result`).
    pub duration_ms: u64,
}

/// One entry of the Messages API `system`/`init` `mcp_servers` list.
#[derive(Clone, Serialize)]
pub(crate) struct McpServer {
    pub name: String,
    pub status: String,
}

/// Folds the agent event stream into NDJSON lines for one wire format.
pub(crate) trait Reducer {
    fn begin(&mut self, _ctx: SessionContext) -> Vec<Value> {
        Vec::new()
    }

    fn reduce(&mut self, event: StreamEvent) -> Vec<Value>;

    fn max_turns(&mut self) -> Vec<Value> {
        Vec::new()
    }

    fn finish(&mut self, end: &TurnEnd<'_>) -> Vec<Value>;

    /// Terminal error line(s). `stop_reason` is a Messages-only override (e.g. `max_tokens`); `None` keeps the fallback.
    fn error(
        &mut self,
        message: &str,
        usage: Option<&Value>,
        duration_ms: u64,
        stop_reason: Option<&str>,
    ) -> Vec<Value>;
}

/// The reducer for `format`, or `None` for `plain`/`json` (rendered directly).
pub(crate) fn reducer_for(format: OutputFormat) -> Option<Box<dyn Reducer>> {
    match format {
        OutputFormat::StreamingJson => Some(Box::new(AcpReducer)),
        OutputFormat::StreamingMessagesJson => Some(Box::new(MessagesReducer::new())),
        OutputFormat::Plain | OutputFormat::Json => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Lifecycle;

    #[test]
    fn memory_plain_messages_do_not_promote_untrusted_diagnostics() {
        let secret = "/Users/alice/private/session.jsonl\nhttps://untrusted.example";
        let flush = Lifecycle::MemoryFlushCompleted {
            result: format!("terminal failure: {secret}"),
            path: Some(secret.to_owned()),
        }
        .plain_message();
        assert_eq!(flush, "Memory flush completed.");
        assert!(!flush.contains(secret));

        let capture = Lifecycle::MemoryCaptureActivity {
            activity: "failed".to_owned(),
            from_turn: 2,
            through_turn: 3,
            attempt: 1,
            detail: Some(secret.to_owned()),
        }
        .plain_message();
        assert_eq!(capture, "Memory capture failed for turns 2-3.");
        assert!(!capture.contains(secret));

        let retry = Lifecycle::MemoryCaptureActivity {
            activity: "failed".to_owned(),
            from_turn: 2,
            through_turn: 3,
            attempt: 2,
            detail: None,
        }
        .plain_message();
        assert_eq!(retry, "Memory capture failed for turns 2-3 (attempt 2).");
    }

    #[test]
    fn unknown_capture_activity_is_not_rendered_verbatim() {
        let message = Lifecycle::MemoryCaptureActivity {
            activity: "https://untrusted.example".to_owned(),
            from_turn: 1,
            through_turn: 1,
            attempt: 1,
            detail: None,
        }
        .plain_message();
        assert_eq!(message, "Memory capture updated for turns 1-1.");
    }
}
