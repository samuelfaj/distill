// Modified for Distill by Samuel Fajreldines, 2026.
//! Wire row + mapper for durable `SessionUpdate::BackgroundTasks` snapshots.
//!
//! Incrementals (`task_backgrounded` / `task_completed`) stay on their own
//! methods; this list is the full-state twin of `SessionUpdate::Plan`.

use chrono::{DateTime, Utc};
use distill_tools::computer::types::TaskKind;
use distill_tools::types::TaskSnapshot;

use crate::extensions::notification::{SessionNotification, SessionUpdate};
use crate::tools::task_completed_frame::{
    FIELD_MAX_BYTES, FRAME_MAX_BYTES, jsonrpc_line_len, prefix_within_encoded_len,
};

/// Client-facing status for one background task in a list snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Running,
    Completed,
    Failed,
}

/// One background task in a durable list snapshot. No stdout — clients that
/// need output still use incrementals / `get_task_output`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BackgroundTaskRow {
    pub task_id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub cwd: String,
    pub kind: TaskKind,
    pub status: BackgroundTaskStatus,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

const SESSION_NOTIFICATION_METHOD: &str = "x.ai/session_notification";

impl BackgroundTaskRow {
    pub(crate) fn from_snapshot(snapshot: TaskSnapshot) -> Self {
        let status = background_task_status(&snapshot);
        let output_file = {
            let path = snapshot.output_file;
            if path.as_os_str().is_empty() {
                None
            } else {
                Some(path.to_string_lossy().into_owned())
            }
        };
        Self {
            task_id: snapshot.task_id,
            command: snapshot.command,
            display_command: snapshot.display_command,
            description: snapshot.description,
            cwd: snapshot.cwd,
            kind: snapshot.kind,
            status,
            started_at: rfc3339(snapshot.start_time),
            ended_at: snapshot.end_time.map(rfc3339),
            output_file,
            exit_code: snapshot.exit_code,
            signal: snapshot.signal,
        }
    }
}

/// Outcome of awaiting the snapshot listing. Distinguishes timeout (skip emit)
/// from a missing backend (`Ok(None)` → authoritative clear).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotListOutcome {
    Tasks(Vec<TaskSnapshot>),
    /// No terminal backend: persist `tasks: []` so replay cannot restore ghosts.
    ClearMissingBackend,
    /// Listing timed out: do not persist an empty clear.
    SkipTimeout,
}

pub(crate) fn snapshot_list_outcome(
    result: Result<Option<Vec<TaskSnapshot>>, ()>,
) -> SnapshotListOutcome {
    match result {
        Ok(Some(tasks)) => SnapshotListOutcome::Tasks(tasks),
        Ok(None) => SnapshotListOutcome::ClearMissingBackend,
        Err(()) => SnapshotListOutcome::SkipTimeout,
    }
}

/// Keep completed tasks still in the registry. Drop foreground-only runs and
/// rows owned by another session. `owner_session_id == None` counts as this
/// session — same rule as the Stop-hook snapshot on a shared terminal.
///
/// Fits the real `SessionNotification` once (same authority as
/// `task_completed_frame::encode`) and returns the wire update directly.
pub(crate) fn background_tasks_update(
    snapshots: impl IntoIterator<Item = TaskSnapshot>,
    session_id: &agent_client_protocol::SessionId,
    meta: Option<serde_json::Value>,
) -> Option<SessionUpdate> {
    let mut tasks: Vec<BackgroundTaskRow> = snapshots
        .into_iter()
        .filter(|task| task.is_backgrounded)
        .filter(|task| {
            task.owner_session_id
                .as_deref()
                .is_none_or(|owner| owner == session_id.0.as_ref())
        })
        .map(BackgroundTaskRow::from_snapshot)
        .collect();

    for row in &mut tasks {
        truncate_row_fields(row);
    }

    // Retention order: running first (newest preferred), then completed/failed
    // (newest preferred). Linear trim drops from the end.
    tasks.sort_by(|left, right| {
        let left_running = left.status == BackgroundTaskStatus::Running;
        let right_running = right.status == BackgroundTaskStatus::Running;
        right_running
            .cmp(&left_running)
            .then_with(|| right.started_at.cmp(&left.started_at))
            .then_with(|| left.task_id.cmp(&right.task_id))
    });

    fit_background_tasks_update(tasks, session_id, meta)
}

fn truncate_row_fields(row: &mut BackgroundTaskRow) {
    row.command = prefix_within_encoded_len(&row.command, FIELD_MAX_BYTES).to_string();
    if let Some(value) = row.display_command.as_mut() {
        *value = prefix_within_encoded_len(value, FIELD_MAX_BYTES).to_string();
    }
    if let Some(value) = row.description.as_mut() {
        *value = prefix_within_encoded_len(value, FIELD_MAX_BYTES).to_string();
    }
    row.cwd = prefix_within_encoded_len(&row.cwd, FIELD_MAX_BYTES).to_string();
}

fn frame_budget() -> usize {
    FRAME_MAX_BYTES.saturating_sub(jsonrpc_line_len(SESSION_NOTIFICATION_METHOD, 0))
}

/// Serialize each row once; track remaining budget while dropping from the end.
fn fit_background_tasks_update(
    tasks: Vec<BackgroundTaskRow>,
    session_id: &agent_client_protocol::SessionId,
    meta: Option<serde_json::Value>,
) -> Option<SessionUpdate> {
    let budget = frame_budget();
    let empty = notification_params(session_id, meta.as_ref(), &[], false)?;
    if empty.get().len() > budget {
        tracing::warn!(
            bytes = empty.get().len(),
            "background_tasks envelope alone exceeds session_notification frame"
        );
        return None;
    }
    let truncated_overhead = {
        let with = notification_params(session_id, meta.as_ref(), &[], true)?;
        with.get().len().saturating_sub(empty.get().len())
    };

    let mut encoded: Vec<(BackgroundTaskRow, usize)> = Vec::with_capacity(tasks.len());
    for row in tasks {
        let len = serde_json::to_vec(&row).ok()?.len();
        encoded.push((row, len));
    }
    let full_len = encoded.len();

    let mut kept: Vec<(BackgroundTaskRow, usize)> = Vec::new();
    let mut used = empty.get().len();
    for (row, row_len) in encoded {
        let extra = if kept.is_empty() {
            row_len
        } else {
            1 + row_len
        };
        if used + extra > budget {
            break;
        }
        used += extra;
        kept.push((row, row_len));
    }

    let mut truncated = kept.len() < full_len;
    if truncated {
        used += truncated_overhead;
        while used > budget {
            let Some((_row, row_len)) = kept.pop() else {
                break;
            };
            let remove = if kept.is_empty() {
                row_len
            } else {
                1 + row_len
            };
            used = used.saturating_sub(remove);
        }
        if used > budget {
            tracing::warn!(
                bytes = used,
                "background_tasks snapshot cannot fit session_notification frame"
            );
            return None;
        }
        truncated = kept.len() < full_len;
    }

    // Final authority: measure the real notification (guards estimate drift).
    let mut tasks: Vec<BackgroundTaskRow> = kept.into_iter().map(|(row, _)| row).collect();
    while !notification_fits(session_id, meta.as_ref(), &tasks, truncated, budget) {
        if tasks.pop().is_none() {
            if truncated {
                return None;
            }
            truncated = true;
            continue;
        }
        truncated = true;
    }

    Some(SessionUpdate::BackgroundTasks { tasks, truncated })
}

fn notification_fits(
    session_id: &agent_client_protocol::SessionId,
    meta: Option<&serde_json::Value>,
    tasks: &[BackgroundTaskRow],
    truncated: bool,
    budget: usize,
) -> bool {
    match notification_params(session_id, meta, tasks, truncated) {
        Some(params) => params.get().len() <= budget,
        None => false,
    }
}

fn notification_params(
    session_id: &agent_client_protocol::SessionId,
    meta: Option<&serde_json::Value>,
    tasks: &[BackgroundTaskRow],
    truncated: bool,
) -> Option<Box<serde_json::value::RawValue>> {
    let notification = SessionNotification {
        session_id: session_id.clone(),
        update: SessionUpdate::BackgroundTasks {
            tasks: tasks.to_vec(),
            truncated,
        },
        meta: meta.cloned(),
    };
    serde_json::value::to_raw_value(&notification).ok()
}

fn background_task_status(snapshot: &TaskSnapshot) -> BackgroundTaskStatus {
    if !snapshot.completed {
        return BackgroundTaskStatus::Running;
    }
    if snapshot.exit_code == Some(0) || (snapshot.exit_code.is_none() && snapshot.signal.is_none())
    {
        BackgroundTaskStatus::Completed
    } else {
        BackgroundTaskStatus::Failed
    }
}

fn rfc3339(time: std::time::SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339()
}

#[cfg(test)]
#[path = "background_task_tests.rs"]
mod tests;
