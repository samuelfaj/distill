// Modified for Distill by Samuel Fajreldines, 2026.
//! Decodes the shell's `x.ai/*` extension notifications into the headless [`ExtEvent`] the orchestrator dispatches.
//! Also answers reverse `ext_method` requests with policy replies.
//! This module owns the wire envelope shapes and the method-to-event mapping, kept out of `headless.rs`.

use agent_client_protocol as acp;
use distill_acp_lib::{AcpArgsBox, AcpResult};

use crate::headless::reducer::{Lifecycle, StreamEvent};

/// Serialize a typed ext-method reply; a serialize failure becomes an explicit ACP error so the oneshot is always answered.
fn ext_response_from<T: serde::Serialize>(value: &T) -> AcpResult<acp::ExtResponse> {
    serde_json::value::to_raw_value(value)
        .map(|raw| acp::ExtResponse::new(raw.into()))
        .map_err(|e| acp::Error::new(-32603, format!("serialize ext response: {e}")))
}

/// Answer a reverse `ext_method` request without a UI.
/// Known interaction methods get a policy reply; dropping `response_tx` instead would fail the whole turn with a channel `recv_failed`.
pub(crate) fn reply_headless_ext_method(args: AcpArgsBox<acp::ExtRequest>) {
    use distill_tools::implementations::distill::ask_user_question::AskUserQuestionExtResponse;
    use distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeExtResponse;

    let method = args.request.method.as_ref();
    // Known methods are answered without parsing params: even a malformed request gets the policy reply rather than a dropped channel
    let response = match method {
        // The model sees the tool's NO_OPERATOR_TEXT (headless sessions are non-interactive), not the interactive "user declined" cancel text
        "x.ai/ask_user_question" => ext_response_from(&AskUserQuestionExtResponse::Cancelled),
        "x.ai/mcp/elicit" => {
            use distill_tools::mcp_elicitation::McpElicitExtResponse;
            ext_response_from(&McpElicitExtResponse::Cancel)
        }
        // The model sees "Your plan has been approved. You can now start coding.".
        "x.ai/exit_plan_mode" => ext_response_from(&ExitPlanModeExtResponse {
            outcome: "approved".to_string(),
            feedback: None,
        }),
        other => Err(acp::Error::new(
            -32601,
            format!("Method not found: {other}"),
        )),
    };
    args.response_tx.send(response).ok();
}

/// Coerce a numeric `task_id` (version skew) to a string so it does not fail the decode and leak an untracked background task.
fn de_task_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "task_id must be a JSON string or number, got {other}"
        ))),
    }
}

fn session_update_tag(params: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(params)
        .ok()?
        .get("update")?
        .get("sessionUpdate")?
        .as_str()
        .map(str::to_string)
}

pub(crate) enum ExtEvent {
    None,
    TaskBackgrounded {
        task_id: String,
        is_monitor: bool,
    },
    TaskCompleted {
        task_id: String,
    },
    SubagentSpawned {
        subagent_id: String,
        attempt_id: Option<String>,
        event_seq: Option<u64>,
    },
    SubagentProgress {
        subagent_id: String,
        attempt_id: Option<String>,
        event_seq: Option<u64>,
    },
    SubagentFinished {
        subagent_id: String,
        attempt_id: Option<String>,
        event_seq: Option<u64>,
    },
    MonitorEvent,
    Lifecycle(Lifecycle),
    Stream(Box<StreamEvent>),
}

pub(crate) fn handle_ext_notification(
    notif: &distill_acp_lib::AcpArgsBox<acp::ExtNotification>,
) -> ExtEvent {
    let method = notif.request.method.as_ref();
    let params = notif.request.params.get();
    if crate::acp::is_session_update_ext_method(method) {
        return decode_session_notification(method, params);
    }
    match method {
        "x.ai/task_backgrounded" => decode_task_backgrounded(method, params),
        "x.ai/task_completed" => decode_task_completed(method, params),
        "x.ai/monitor_event" => ExtEvent::MonitorEvent,
        "x.ai/leader/version_mismatch" => {
            match crate::acp::version_mismatch_banner(params) {
                Some(banner) => tracing::warn!(%banner, "x.ai/leader/version_mismatch"),
                None => {
                    tracing::warn!("ignoring x.ai/leader/version_mismatch without usable versions")
                }
            }
            ExtEvent::None
        }
        _ => ExtEvent::None,
    }
}

fn decode_task_backgrounded(method: &str, params: &str) -> ExtEvent {
    #[derive(serde::Deserialize)]
    struct TaskBgEnvelope {
        update: TaskBgUpdate,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case", tag = "sessionUpdate")]
    enum TaskBgUpdate {
        TaskBackgrounded {
            #[serde(deserialize_with = "de_task_id")]
            task_id: String,
            #[serde(default)]
            monitor_description: Option<String>,
        },
        #[serde(other)]
        Other,
    }
    match serde_json::from_str::<TaskBgEnvelope>(params) {
        Ok(env) => match env.update {
            TaskBgUpdate::TaskBackgrounded {
                task_id,
                monitor_description,
            } => ExtEvent::TaskBackgrounded {
                task_id,
                is_monitor: monitor_description.is_some(),
            },
            // The sessionUpdate tag does not match the method; log loudly instead of silently dropping
            TaskBgUpdate::Other => {
                tracing::error!(
                    method,
                    payload = params,
                    "headless: x.ai/task_backgrounded with mismatched sessionUpdate \
                     tag; background task will not be tracked for reaping"
                );
                ExtEvent::None
            }
        },
        Err(e) => {
            tracing::error!(
                method,
                error = %e,
                payload = params,
                "headless: undecodable x.ai/task_backgrounded notification; \
                 background task will not be tracked for reaping"
            );
            ExtEvent::None
        }
    }
}

fn decode_task_completed(method: &str, params: &str) -> ExtEvent {
    #[derive(serde::Deserialize)]
    struct TaskDoneEnvelope {
        update: TaskDoneUpdate,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case", tag = "sessionUpdate")]
    enum TaskDoneUpdate {
        TaskCompleted {
            task_snapshot: TaskSnapshotLite,
        },
        #[serde(other)]
        Other,
    }
    #[derive(serde::Deserialize)]
    struct TaskSnapshotLite {
        #[serde(deserialize_with = "de_task_id")]
        task_id: String,
    }
    match serde_json::from_str::<TaskDoneEnvelope>(params) {
        Ok(env) => match env.update {
            TaskDoneUpdate::TaskCompleted { task_snapshot } => ExtEvent::TaskCompleted {
                task_id: task_snapshot.task_id,
            },
            // The sessionUpdate tag does not match the method; log loudly instead of silently dropping
            TaskDoneUpdate::Other => {
                tracing::error!(
                    method,
                    payload = params,
                    "headless: x.ai/task_completed with mismatched sessionUpdate \
                     tag; background task completion will not be recorded"
                );
                ExtEvent::None
            }
        },
        Err(e) => {
            tracing::error!(
                method,
                error = %e,
                payload = params,
                "headless: undecodable x.ai/task_completed notification; \
                 background task completion will not be recorded"
            );
            ExtEvent::None
        }
    }
}

fn decode_session_notification(method: &str, params: &str) -> ExtEvent {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case", tag = "sessionUpdate")]
    enum XaiUpdate {
        AutoCompactStarted {
            percentage: u8,
        },
        AutoCompactCompleted {
            #[serde(default)]
            tokens_before: Option<u64>,
        },
        AutoCompactFailed {
            error: String,
        },
        AutoCompactCancelled {},
        AutoContinueCompleted {
            total_tokens: u64,
        },
        ImageCompressed {
            message: String,
        },
        MemoryFlushStarted {},
        MemoryFlushCompleted {
            result: String,
            #[serde(default)]
            path: Option<String>,
        },
        MemoryCaptureActivity {
            activity: String,
            from_turn: u32,
            through_turn: u32,
            attempt: u32,
            #[serde(default)]
            detail: Option<String>,
        },
        SubagentSpawned {
            subagent_id: String,
            #[serde(default)]
            attempt_id: Option<String>,
        },
        SubagentProgress {
            subagent_id: String,
            #[serde(default)]
            attempt_id: Option<String>,
        },
        SubagentFinished {
            subagent_id: String,
            #[serde(default)]
            attempt_id: Option<String>,
        },
        ResponseStarted {
            #[serde(default)]
            message_id: Option<String>,
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            input_tokens: u64,
            #[serde(default)]
            cache_read_input_tokens: u64,
            #[serde(default)]
            cache_creation_input_tokens: u64,
        },
        ReasoningCompleted {
            #[serde(default)]
            signature: Option<String>,
        },
        ResponseCompleted {
            #[serde(default)]
            message_id: Option<String>,
            #[serde(default)]
            stop_reason: Option<String>,
            #[serde(default)]
            usage: Option<distill_shell::extensions::notification::ResponseUsage>,
            #[serde(default)]
            signature: Option<String>,
            #[serde(default)]
            stop_sequence: Option<String>,
        },
        #[serde(other)]
        Other,
    }
    #[derive(serde::Deserialize)]
    struct XaiNotif {
        update: XaiUpdate,
        #[serde(default, rename = "_meta")]
        meta: Option<serde_json::Value>,
    }

    let xai_notif = match serde_json::from_str::<XaiNotif>(params) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                method,
                error = %e,
                "headless: malformed x.ai session notification; ignoring"
            );
            return ExtEvent::None;
        }
    };

    let event_seq = xai_notif
        .meta
        .as_ref()
        .and_then(|meta| meta.get("eventId"))
        .and_then(serde_json::Value::as_str)
        .and_then(crate::acp::meta::event_id_counter);

    match xai_notif.update {
        XaiUpdate::AutoCompactStarted { percentage } => {
            ExtEvent::Lifecycle(Lifecycle::CompactStarted { percentage })
        }
        XaiUpdate::AutoCompactCompleted { tokens_before } => {
            ExtEvent::Lifecycle(Lifecycle::CompactCompleted {
                pre_tokens: tokens_before.unwrap_or(0),
            })
        }
        XaiUpdate::AutoCompactFailed { error } => {
            ExtEvent::Lifecycle(Lifecycle::CompactFailed { error })
        }
        XaiUpdate::AutoCompactCancelled {} => ExtEvent::Lifecycle(Lifecycle::CompactCancelled),
        XaiUpdate::AutoContinueCompleted { total_tokens } => {
            ExtEvent::Lifecycle(Lifecycle::AutoContinue { total_tokens })
        }
        XaiUpdate::ImageCompressed { message } => {
            ExtEvent::Lifecycle(Lifecycle::ImageCompressed { message })
        }
        XaiUpdate::MemoryFlushStarted {} => ExtEvent::Lifecycle(Lifecycle::MemoryFlushStarted),
        XaiUpdate::MemoryFlushCompleted { result, path } => {
            ExtEvent::Lifecycle(Lifecycle::MemoryFlushCompleted { result, path })
        }
        XaiUpdate::MemoryCaptureActivity {
            activity,
            from_turn,
            through_turn,
            attempt,
            detail,
        } => ExtEvent::Lifecycle(Lifecycle::MemoryCaptureActivity {
            activity,
            from_turn,
            through_turn,
            attempt,
            detail,
        }),
        XaiUpdate::SubagentSpawned {
            subagent_id,
            attempt_id,
        } => ExtEvent::SubagentSpawned {
            subagent_id,
            attempt_id,
            event_seq,
        },
        XaiUpdate::SubagentProgress {
            subagent_id,
            attempt_id,
        } => ExtEvent::SubagentProgress {
            subagent_id,
            attempt_id,
            event_seq,
        },
        XaiUpdate::SubagentFinished {
            subagent_id,
            attempt_id,
        } => ExtEvent::SubagentFinished {
            subagent_id,
            attempt_id,
            event_seq,
        },
        XaiUpdate::ResponseStarted {
            message_id,
            model,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        } => ExtEvent::Stream(Box::new(StreamEvent::ResponseStarted {
            message_id,
            model,
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        })),
        XaiUpdate::ReasoningCompleted { signature } => {
            ExtEvent::Stream(Box::new(StreamEvent::ReasoningCompleted { signature }))
        }
        XaiUpdate::ResponseCompleted {
            message_id,
            stop_reason,
            usage,
            signature,
            stop_sequence,
        } => ExtEvent::Stream(Box::new(StreamEvent::ResponseCompleted {
            message_id,
            stop_reason,
            usage,
            signature,
            stop_sequence,
        })),
        // A task_backgrounded or task_completed tag arriving here belongs on its dedicated method; log loudly
        // Any other unknown tag stays a clean ignore
        XaiUpdate::Other => {
            if let Some(tag) = session_update_tag(params)
                && matches!(tag.as_str(), "task_backgrounded" | "task_completed")
            {
                tracing::error!(
                    method,
                    tag,
                    payload = params,
                    "headless: background-task lifecycle tag on a session notification \
                     (expected the dedicated x.ai/task_backgrounded|task_completed method); \
                     background tracking will not be updated"
                );
            }
            ExtEvent::None
        }
    }
}

#[cfg(test)]
#[path = "ext_protocol_tests.rs"]
mod tests;
