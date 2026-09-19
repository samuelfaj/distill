// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::PathBuf;
use std::time::SystemTime;

use distill_tools::computer::types::TaskKind;
use distill_tools::types::TaskSnapshot;
use serde_json::json;

use super::{
    BackgroundTaskRow, BackgroundTaskStatus, SnapshotListOutcome, background_tasks_update,
    snapshot_list_outcome,
};
use crate::extensions::notification::{SessionNotification, SessionUpdate};
use crate::tools::task_completed_frame::{FRAME_MAX_BYTES, jsonrpc_line_len};

fn snapshot(completed: bool, exit_code: Option<i32>, signal: Option<&str>) -> TaskSnapshot {
    TaskSnapshot {
        task_id: "bg-1".into(),
        command: "sleep 10".into(),
        display_command: Some("sleep 10".into()),
        cwd: "/tmp".into(),
        start_time: SystemTime::UNIX_EPOCH,
        end_time: completed.then_some(SystemTime::UNIX_EPOCH),
        output: "should never appear on the wire".into(),
        output_file: PathBuf::from("/tmp/bg-1.log"),
        truncated: false,
        exit_code,
        signal: signal.map(str::to_string),
        completed,
        kind: TaskKind::Bash,
        block_waited: false,
        explicitly_killed: false,
        kill_result_delivered: false,
        owner_session_id: None,
        description: Some("wait".into()),
        is_backgrounded: true,
        output_total_bytes: 32,
    }
}

fn sess() -> agent_client_protocol::SessionId {
    agent_client_protocol::SessionId::new("sess-a")
}

fn meta() -> Option<serde_json::Value> {
    Some(json!({
        "eventId": "e".repeat(80),
        "agentTimestampMs": 1_700_000_000_000i64,
    }))
}

fn update_tasks(update: SessionUpdate) -> (Vec<BackgroundTaskRow>, bool) {
    match update {
        SessionUpdate::BackgroundTasks { tasks, truncated } => (tasks, truncated),
        other => panic!("expected BackgroundTasks, got {other:?}"),
    }
}

#[test]
fn timeout_skips_emit_while_missing_backend_clears() {
    assert_eq!(
        snapshot_list_outcome(Err(())),
        SnapshotListOutcome::SkipTimeout
    );
    assert_eq!(
        snapshot_list_outcome(Ok(None)),
        SnapshotListOutcome::ClearMissingBackend
    );
    assert!(matches!(
        snapshot_list_outcome(Ok(Some(vec![snapshot(false, None, None)]))),
        SnapshotListOutcome::Tasks(_)
    ));
}

#[test]
fn running_when_not_completed() {
    let row = BackgroundTaskRow::from_snapshot(snapshot(false, None, None));
    assert_eq!(row.status, BackgroundTaskStatus::Running);
    assert!(row.ended_at.is_none());
}

#[test]
fn completed_on_exit_zero() {
    let row = BackgroundTaskRow::from_snapshot(snapshot(true, Some(0), None));
    assert_eq!(row.status, BackgroundTaskStatus::Completed);
}

#[test]
fn completed_when_exited_without_code_or_signal() {
    let row = BackgroundTaskRow::from_snapshot(snapshot(true, None, None));
    assert_eq!(row.status, BackgroundTaskStatus::Completed);
}

#[test]
fn failed_on_nonzero_exit() {
    let row = BackgroundTaskRow::from_snapshot(snapshot(true, Some(1), None));
    assert_eq!(row.status, BackgroundTaskStatus::Failed);
}

#[test]
fn failed_on_session_restart_signal() {
    let row = BackgroundTaskRow::from_snapshot(snapshot(true, None, Some("session_restart")));
    assert_eq!(row.status, BackgroundTaskStatus::Failed);
    assert_eq!(row.signal.as_deref(), Some("session_restart"));
}

#[test]
fn membership_is_backgrounded_only_and_keeps_completed() {
    let mut fg = snapshot(false, None, None);
    fg.task_id = "fg".into();
    fg.is_backgrounded = false;
    let mut done = snapshot(true, Some(0), None);
    done.task_id = "done".into();
    let update = background_tasks_update([fg, done], &sess(), meta()).expect("fit");
    let (tasks, truncated) = update_tasks(update);
    assert_eq!(tasks.len(), 1);
    let Some(task) = tasks.first() else {
        panic!("expected one task: {tasks:?}");
    };
    assert_eq!(task.task_id, "done");
    assert!(!truncated);
}

#[test]
fn session_update_tag_is_background_tasks_and_omits_output() {
    let update = SessionUpdate::BackgroundTasks {
        tasks: vec![BackgroundTaskRow::from_snapshot(snapshot(
            true,
            Some(0),
            None,
        ))],
        truncated: false,
    };
    let value = serde_json::to_value(&update).expect("serialize");
    assert_eq!(
        value.get("sessionUpdate").and_then(|v| v.as_str()),
        Some("background_tasks")
    );
    assert!(value.get("output").is_none());
    let Some(task) = value
        .get("tasks")
        .and_then(|t| t.as_array())
        .and_then(|a| a.first())
    else {
        panic!("expected one serialized task: {value:?}");
    };
    assert!(task.get("output").is_none());
    assert_eq!(task.get("task_id").and_then(|v| v.as_str()), Some("bg-1"));
    assert_eq!(
        task.get("status").and_then(|v| v.as_str()),
        Some("completed")
    );
    assert_eq!(task.get("kind").and_then(|v| v.as_str()), Some("bash"));
    assert_eq!(
        task.get("started_at").and_then(|v| v.as_str()),
        Some("1970-01-01T00:00:00+00:00")
    );
    assert_eq!(
        task.get("output_file").and_then(|v| v.as_str()),
        Some("/tmp/bg-1.log")
    );
    assert!(task.get("tool_call_id").is_none());
    assert!(value.get("omitted_count").is_none());

    let roundtrip: SessionUpdate = serde_json::from_value(value).expect("deserialize");
    assert!(matches!(roundtrip, SessionUpdate::BackgroundTasks { .. }));
}

#[test]
fn empty_list_is_valid_wire() {
    let update = SessionUpdate::BackgroundTasks {
        tasks: vec![],
        truncated: false,
    };
    let value = serde_json::to_value(&update).expect("serialize");
    assert_eq!(
        value,
        json!({
            "sessionUpdate": "background_tasks",
            "tasks": []
        })
    );
}

#[test]
fn empty_output_file_is_omitted() {
    let mut snap = snapshot(true, Some(0), None);
    snap.output_file = PathBuf::new();
    let value = serde_json::to_value(BackgroundTaskRow::from_snapshot(snap)).expect("serialize");
    assert!(value.get("output_file").is_none());
}

#[test]
fn session_scoped_list_drops_other_owners_and_keeps_unowned() {
    let mut mine = snapshot(false, None, None);
    mine.task_id = "mine".into();
    mine.owner_session_id = Some("sess-a".into());
    let mut sibling = snapshot(false, None, None);
    sibling.task_id = "sib".into();
    sibling.owner_session_id = Some("sess-b".into());
    let mut unowned = snapshot(false, None, None);
    unowned.task_id = "none".into();
    unowned.owner_session_id = None;
    let update = background_tasks_update([mine, sibling, unowned], &sess(), meta()).expect("fit");
    let (tasks, _) = update_tasks(update);
    let ids: Vec<_> = tasks.iter().map(|r| r.task_id.as_str()).collect();
    assert_eq!(ids, ["mine", "none"]);
}

fn session_notification_frame_len(update: &SessionUpdate) -> usize {
    let notification = SessionNotification {
        session_id: sess(),
        update: update.clone(),
        meta: meta(),
    };
    let params = serde_json::to_vec(&notification).expect("serialize");
    jsonrpc_line_len("x.ai/session_notification", params.len())
}

#[test]
fn fitted_list_stays_within_task_completed_frame_budget() {
    let mut huge = snapshot(true, Some(0), None);
    huge.command = "x".repeat(64 * 1024);
    huge.task_id = "huge".into();
    let update = background_tasks_update([huge], &sess(), meta()).expect("fit");
    let (tasks, truncated) = update_tasks(update.clone());
    assert_eq!(tasks.len(), 1);
    let Some(task) = tasks.first() else {
        panic!("expected one task: {tasks:?}");
    };
    assert!(task.command.len() <= 1024);
    assert!(!truncated);
    let line = session_notification_frame_len(&update);
    assert!(line <= FRAME_MAX_BYTES, "line is {line} bytes");
}

#[test]
fn running_only_oversized_list_is_trimmed_to_the_session_notification_frame() {
    let snaps: Vec<_> = (0..80)
        .map(|i| {
            let mut snap = snapshot(false, None, None);
            snap.task_id = format!("run-{i:02}");
            snap.start_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i);
            snap.command = "x".repeat(2048);
            snap.display_command = Some("y".repeat(2048));
            snap.description = Some("z".repeat(2048));
            snap.cwd = "w".repeat(2048);
            snap
        })
        .rev()
        .collect();
    let update = background_tasks_update(snaps, &sess(), meta()).expect("fit");
    let (tasks, truncated) = update_tasks(update.clone());
    assert!(!tasks.is_empty());
    assert!(tasks.len() < 80);
    assert!(truncated);
    assert!(
        tasks
            .iter()
            .all(|row| row.status == BackgroundTaskStatus::Running)
    );
    // Newest running retained first.
    let nums: Vec<usize> = tasks
        .iter()
        .map(|row| {
            row.task_id
                .strip_prefix("run-")
                .and_then(|id| id.parse().ok())
                .expect("padded run id")
        })
        .collect();
    assert!(
        nums.windows(2).all(|w| matches!(w, [a, b] if a >= b)),
        "expected newest-first running retention, got {nums:?}"
    );
    let line = session_notification_frame_len(&update);
    assert!(line <= FRAME_MAX_BYTES, "line is {line} bytes");
}

#[test]
fn truncated_marker_is_on_the_wire_when_rows_are_dropped() {
    let snaps: Vec<_> = (0..80)
        .map(|i| {
            let mut snap = snapshot(false, None, None);
            snap.task_id = format!("run-{i:02}");
            snap.start_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i);
            snap.command = "x".repeat(2048);
            snap.display_command = Some("y".repeat(2048));
            snap.description = Some("z".repeat(2048));
            snap.cwd = "w".repeat(2048);
            snap
        })
        .collect();
    let update = background_tasks_update(snaps, &sess(), meta()).expect("fit");
    let value = serde_json::to_value(&update).expect("serialize");
    assert_eq!(value.get("truncated").and_then(|v| v.as_bool()), Some(true));
    assert!(value.get("omitted_count").is_none());
}

#[test]
fn prefers_running_over_completed_when_trimming() {
    let mut running = snapshot(false, None, None);
    running.task_id = "run".into();
    running.start_time = SystemTime::UNIX_EPOCH;
    running.command = "x".repeat(2048);
    running.cwd = "w".repeat(2048);
    let completed: Vec<_> = (0..60)
        .map(|i| {
            let mut snap = snapshot(true, Some(0), None);
            snap.task_id = format!("done-{i:02}");
            snap.start_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i + 1);
            snap.command = "x".repeat(2048);
            snap.cwd = "w".repeat(2048);
            snap
        })
        .collect();
    let mut snaps = completed;
    snaps.push(running);
    let update = background_tasks_update(snaps, &sess(), meta()).expect("fit");
    let (tasks, truncated) = update_tasks(update);
    assert!(truncated);
    assert!(
        tasks.iter().any(|row| row.task_id == "run"),
        "running row must be retained: {tasks:?}"
    );
}
