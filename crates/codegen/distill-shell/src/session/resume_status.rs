// Modified for Distill by Samuel Fajreldines, 2026.
//! Snapshot of live work written before session teardown, consumed once on
//! the first prompt after a cold resume so the model gets a status update.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const FILENAME: &str = "resume_status.json";

pub(crate) const ONESHOT_TIMEOUT: Duration = Duration::from_millis(200);
pub(crate) const PERSIST_ACK_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeStatusSnapshot {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<ResumeLoop>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background: Vec<ResumeTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<ResumeTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<ResumeSubagent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<ResumeWorkflow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<ResumeGoal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeLoop {
    pub id: String,
    pub interval_secs: u64,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResumeTask {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl<'de> Deserialize<'de> for ResumeTask {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            task_id: String,
            #[serde(default)]
            command: String,
            #[serde(default)]
            description: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(ResumeTask {
            task_id: raw.task_id,
            description: resume_task_description(raw.description, Some(raw.command.as_str())),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeSubagent {
    pub subagent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeWorkflow {
    pub run_id: String,
    pub objective: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeGoal {
    pub objective: String,
}

impl ResumeStatusSnapshot {
    pub(crate) fn is_empty(&self) -> bool {
        self.loops.is_empty()
            && self.background.is_empty()
            && self.monitors.is_empty()
            && self.subagents.is_empty()
            && self.workflows.is_empty()
            && self.goal.is_none()
    }
}

pub(crate) fn persist(session_dir: &Path, snapshot: &ResumeStatusSnapshot) {
    if snapshot.is_empty() {
        return;
    }
    let path = session_dir.join(FILENAME);
    let Ok(data) = serde_json::to_vec_pretty(snapshot) else {
        tracing::warn!("failed to serialize resume status snapshot");
        return;
    };
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(e) = std::io::Write::write_all(&mut file, &data) {
                tracing::warn!(%e, "failed to write resume status snapshot");
                let _ = std::fs::remove_file(&path);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => tracing::warn!(%e, "failed to create resume status snapshot"),
    }
}

pub(crate) fn exists(session_dir: &Path) -> bool {
    session_dir.join(FILENAME).is_file()
}

pub(crate) fn load_and_clear(session_dir: &Path) -> ResumeStatusSnapshot {
    let path = session_dir.join(FILENAME);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => return merge_legacy_manifest(session_dir, ResumeStatusSnapshot::default()),
    };
    let snapshot = match serde_json::from_slice(&data) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "failed to parse resume status snapshot");
            let _ = std::fs::remove_file(&path);
            return merge_legacy_manifest(session_dir, ResumeStatusSnapshot::default());
        }
    };
    let _ = std::fs::remove_file(&path);
    merge_legacy_manifest(session_dir, snapshot)
}

pub(crate) fn resume_task_description(
    description: Option<String>,
    display_command: Option<&str>,
) -> Option<String> {
    if let Some(desc) = description.filter(|d| !d.trim().is_empty()) {
        return Some(desc);
    }
    display_command
        .and_then(distill_tools::reminders::monitor_label)
        .map(str::to_string)
}

fn merge_legacy_manifest(
    session_dir: &Path,
    mut snapshot: ResumeStatusSnapshot,
) -> ResumeStatusSnapshot {
    let entries = crate::terminal::load_and_clear_manifest(session_dir);
    for entry in entries {
        let description = resume_task_description(None, entry.display_command.as_deref());
        let task = ResumeTask {
            task_id: entry.task_id,
            description,
        };
        match entry.kind {
            distill_tools::computer::types::TaskKind::Monitor => {
                if !snapshot.monitors.iter().any(|t| t.task_id == task.task_id) {
                    snapshot.monitors.push(task);
                }
            }
            distill_tools::computer::types::TaskKind::Bash => {
                if !snapshot
                    .background
                    .iter()
                    .any(|t| t.task_id == task.task_id)
                {
                    snapshot.background.push(task);
                }
            }
        }
    }
    snapshot
}

/// Reconstruct a snapshot from leftover session files when teardown did not
/// write one (crash). Bash/monitors cannot be recovered this way.
pub(crate) fn reconstruct_from_disk(
    session_dir: &Path,
    parent_session_id: &str,
    workflows: impl IntoIterator<Item = ResumeWorkflow>,
    goal: Option<ResumeGoal>,
) -> ResumeStatusSnapshot {
    ResumeStatusSnapshot {
        loops: loops_from_resources_state(session_dir),
        background: Vec::new(),
        monitors: Vec::new(),
        subagents: running_subagent_metas(session_dir, parent_session_id),
        workflows: workflows.into_iter().collect(),
        goal,
    }
}

pub(crate) fn loops_from_resources_state(session_dir: &Path) -> Vec<ResumeLoop> {
    let path = session_dir.join("resources_state.json");
    let Ok(data) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data) else {
        return Vec::new();
    };
    let Some(tasks) = value
        .pointer("/state/distill.Scheduler/tasks")
        .and_then(|t| t.as_array())
    else {
        return Vec::new();
    };
    let now = chrono::Utc::now();
    tasks
        .iter()
        .filter(|task| loop_still_scheduled(task, now))
        .filter_map(|task| {
            Some(ResumeLoop {
                id: task.get("id")?.as_str()?.to_string(),
                interval_secs: task.get("intervalSecs")?.as_u64()?,
                prompt: task.get("prompt")?.as_str()?.to_string(),
            })
        })
        .collect()
}

fn loop_still_scheduled(task: &serde_json::Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    if let Some(exp) = task
        .get("expiresAt")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        && now >= exp.with_timezone(&chrono::Utc)
    {
        return false;
    }
    let recurring = task
        .get("recurring")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let already_fired = task.get("lastFiredAt").is_some_and(|v| !v.is_null());
    recurring || !already_fired
}

pub(crate) fn running_subagent_metas(
    session_dir: &Path,
    parent_session_id: &str,
) -> Vec<ResumeSubagent> {
    let dir = session_dir.join("subagents");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let meta_path = entry.path().join("meta.json");
        let Ok(data) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        if meta.get("status").and_then(|s| s.as_str()) != Some("running") {
            continue;
        }
        if meta.get("parent_session_id").and_then(|s| s.as_str()) != Some(parent_session_id) {
            continue;
        }
        let Some(subagent_id) = meta
            .get("subagent_id")
            .and_then(|s| s.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        out.push(ResumeSubagent {
            subagent_id,
            subagent_type: meta
                .get("subagent_type")
                .and_then(|s| s.as_str())
                .map(str::to_string),
            description: meta
                .get("description")
                .and_then(|s| s.as_str())
                .map(str::to_string),
        });
    }
    out
}

fn write_resumed_task_line(body: &mut String, item: &ResumeTask) {
    let desc = item
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let _ = match desc {
        Some(desc) => std::fmt::Write::write_fmt(
            body,
            format_args!("- \"{}\": {}\n", item.task_id, one_line(desc)),
        ),
        None => std::fmt::Write::write_fmt(body, format_args!("- \"{}\"\n", item.task_id)),
    };
}

pub(crate) fn format_reminder(snapshot: &ResumeStatusSnapshot) -> Option<String> {
    if snapshot.is_empty() {
        return None;
    }
    let mut body = String::from("This session was resumed after the previous process exited.\n");
    if !snapshot.loops.is_empty() {
        body.push_str("\n## Loops\nThese loops are still scheduled despite the restart:\n");
        for item in &snapshot.loops {
            let _ = std::fmt::Write::write_fmt(
                &mut body,
                format_args!(
                    "- \"{}\": every {}s — {}\n",
                    item.id,
                    item.interval_secs,
                    one_line(&item.prompt)
                ),
            );
        }
    }
    if !snapshot.background.is_empty() {
        body.push_str(
            "\n## Background commands\nThese background commands were killed when the session stopped:\n",
        );
        for item in &snapshot.background {
            write_resumed_task_line(&mut body, item);
        }
    }
    if !snapshot.monitors.is_empty() {
        body.push_str("\n## Monitors\nThese monitors were killed when the session stopped:\n");
        for item in &snapshot.monitors {
            write_resumed_task_line(&mut body, item);
        }
    }
    if !snapshot.subagents.is_empty() {
        body.push_str("\n## Subagents\nThese subagents were cancelled when the session stopped:\n");
        for item in &snapshot.subagents {
            match (&item.subagent_type, &item.description) {
                (Some(ty), Some(desc)) => {
                    let _ = std::fmt::Write::write_fmt(
                        &mut body,
                        format_args!("- \"{}\" ({ty}): {}\n", item.subagent_id, one_line(desc)),
                    );
                }
                (Some(ty), None) => {
                    let _ = std::fmt::Write::write_fmt(
                        &mut body,
                        format_args!("- \"{}\" ({ty})\n", item.subagent_id),
                    );
                }
                (None, Some(desc)) => {
                    let _ = std::fmt::Write::write_fmt(
                        &mut body,
                        format_args!("- \"{}\": {}\n", item.subagent_id, one_line(desc)),
                    );
                }
                (None, None) => {
                    let _ = std::fmt::Write::write_fmt(
                        &mut body,
                        format_args!("- \"{}\"\n", item.subagent_id),
                    );
                }
            }
        }
    }
    if !snapshot.workflows.is_empty() {
        body.push_str("\n## Workflows\nThese workflows were cancelled when the session stopped:\n");
        for item in &snapshot.workflows {
            let _ = std::fmt::Write::write_fmt(
                &mut body,
                format_args!(
                    "- \"{}\" (cancelled): {}\n",
                    item.run_id,
                    one_line(&item.objective)
                ),
            );
        }
    }
    if let Some(goal) = &snapshot.goal {
        body.push_str(
            "\n## Goal\nThe goal was paused when the session stopped and can be resumed:\n",
        );
        let _ = std::fmt::Write::write_fmt(
            &mut body,
            format_args!("- {}\n", one_line(&goal.objective)),
        );
    }
    Some(body)
}

fn one_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_skips_empty() {
        assert!(format_reminder(&ResumeStatusSnapshot::default()).is_none());
    }

    #[test]
    fn load_migrates_monitor_label_out_of_command() {
        let raw = r#"{"task_id":"mon1","command":"[monitor] app logs"}"#;
        let task: ResumeTask = serde_json::from_str(raw).unwrap();
        assert_eq!(task.description.as_deref(), Some("app logs"));
        let bash: ResumeTask =
            serde_json::from_str(r#"{"task_id":"bg1","command":"sleep 2 && echo hi"}"#).unwrap();
        assert!(bash.description.is_none());
        let blank: ResumeTask = serde_json::from_str(
            r#"{"task_id":"mon2","command":"[monitor] app logs","description":"  "}"#,
        )
        .unwrap();
        assert_eq!(blank.description.as_deref(), Some("app logs"));
        let kept: ResumeTask = serde_json::from_str(
            r#"{"task_id":"bg2","command":"sleep 2","description":"Print smoke marker"}"#,
        )
        .unwrap();
        assert_eq!(kept.description.as_deref(), Some("Print smoke marker"));
    }

    #[test]
    fn format_covers_each_section() {
        let reminder = format_reminder(&ResumeStatusSnapshot {
            loops: vec![ResumeLoop {
                id: "loop1".into(),
                interval_secs: 1200,
                prompt: "babysit".into(),
            }],
            background: vec![ResumeTask {
                task_id: "bg1".into(),
                description: Some("Wait for the deploy".into()),
            }],
            monitors: vec![ResumeTask {
                task_id: "mon1".into(),
                description: None,
            }],
            subagents: vec![ResumeSubagent {
                subagent_id: "sa1".into(),
                subagent_type: Some("general-purpose".into()),
                description: Some("review".into()),
            }],
            workflows: vec![ResumeWorkflow {
                run_id: "wf1".into(),
                objective: "ship it".into(),
            }],
            goal: Some(ResumeGoal {
                objective: "land the PR".into(),
            }),
        })
        .expect("non-empty");
        assert!(reminder.contains("This session was resumed after the previous process exited."));
        assert!(reminder.contains("still scheduled despite the restart"));
        assert!(reminder.contains("background commands were killed"));
        assert!(reminder.contains("monitors were killed"));
        assert!(reminder.contains("subagents were cancelled"));
        assert!(reminder.contains("These workflows were cancelled when the session stopped:"));
        assert!(reminder.contains("goal was paused"));
        assert!(reminder.contains("loop1"));
        assert!(reminder.contains("bg1"));
        assert!(reminder.contains("Wait for the deploy"));
        assert!(reminder.contains("mon1"));
        assert!(reminder.contains("sa1"));
        assert!(reminder.contains("wf1"));
        assert!(reminder.contains("land the PR"));
    }

    #[test]
    fn persist_roundtrip_then_clear() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = ResumeStatusSnapshot {
            loops: vec![ResumeLoop {
                id: "loop1".into(),
                interval_secs: 60,
                prompt: "tick".into(),
            }],
            ..ResumeStatusSnapshot::default()
        };
        persist(dir.path(), &snapshot);
        assert!(exists(dir.path()));
        let loaded = load_and_clear(dir.path());
        assert_eq!(loaded, snapshot);
        assert!(!exists(dir.path()));
        assert!(load_and_clear(dir.path()).is_empty());
    }

    #[test]
    fn persist_skips_empty() {
        let dir = tempfile::tempdir().unwrap();
        persist(dir.path(), &ResumeStatusSnapshot::default());
        assert!(!exists(dir.path()));
    }

    #[test]
    fn persist_keeps_first_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        persist(
            dir.path(),
            &ResumeStatusSnapshot {
                background: vec![ResumeTask {
                    task_id: "bg1".into(),
                    description: None,
                }],
                ..ResumeStatusSnapshot::default()
            },
        );
        persist(
            dir.path(),
            &ResumeStatusSnapshot {
                loops: vec![ResumeLoop {
                    id: "loop1".into(),
                    interval_secs: 60,
                    prompt: "tick".into(),
                }],
                ..ResumeStatusSnapshot::default()
            },
        );
        let loaded = load_and_clear(dir.path());
        assert_eq!(loaded.background.len(), 1);
        assert!(loaded.loops.is_empty());
    }

    #[test]
    fn load_and_clear_deletes_corrupt_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("resume_status.json"), "not-json").unwrap();
        let loaded = load_and_clear(dir.path());
        assert!(loaded.is_empty());
        assert!(!exists(dir.path()));
        persist(
            dir.path(),
            &ResumeStatusSnapshot {
                loops: vec![ResumeLoop {
                    id: "loop1".into(),
                    interval_secs: 60,
                    prompt: "tick".into(),
                }],
                ..ResumeStatusSnapshot::default()
            },
        );
        assert!(exists(dir.path()));
    }

    #[test]
    fn reconstruct_reads_scheduler_and_running_meta() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("resources_state.json"),
            r#"{
              "state": {
                "distill.Scheduler": {
                  "tasks": [
                    {"id":"loop9","intervalSecs":30,"prompt":"ping"}
                  ]
                }
              }
            }"#,
        )
        .unwrap();
        let meta_dir = dir.path().join("subagents").join("sa9");
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(
            meta_dir.join("meta.json"),
            r#"{"subagent_id":"sa9","parent_session_id":"sess","status":"running","subagent_type":"explore","description":"look"}"#,
        )
        .unwrap();
        let snap = reconstruct_from_disk(
            dir.path(),
            "sess",
            [ResumeWorkflow {
                run_id: "wf9".into(),
                objective: "do".into(),
            }],
            Some(ResumeGoal {
                objective: "goal".into(),
            }),
        );
        let Some(loop0) = snap.loops.first() else {
            panic!("expected a loop: {:?}", snap.loops);
        };
        assert_eq!(loop0.id, "loop9");
        let Some(sa0) = snap.subagents.first() else {
            panic!("expected a subagent: {:?}", snap.subagents);
        };
        assert_eq!(sa0.subagent_id, "sa9");
        let Some(wf0) = snap.workflows.first() else {
            panic!("expected a workflow: {:?}", snap.workflows);
        };
        assert_eq!(wf0.run_id, "wf9");
        assert_eq!(snap.goal.unwrap().objective, "goal");
    }

    #[test]
    fn format_calls_every_workflow_cancelled() {
        let reminder = format_reminder(&ResumeStatusSnapshot {
            workflows: vec![
                ResumeWorkflow {
                    run_id: "wf-paused".into(),
                    objective: "wait".into(),
                },
                ResumeWorkflow {
                    run_id: "wf-dead".into(),
                    objective: "cut off".into(),
                },
            ],
            ..ResumeStatusSnapshot::default()
        })
        .expect("non-empty");
        assert_eq!(
            reminder
                .matches("These workflows were cancelled when the session stopped:")
                .count(),
            1
        );
        assert!(reminder.contains("\"wf-paused\" (cancelled)"));
        assert!(reminder.contains("\"wf-dead\" (cancelled)"));
    }

    #[test]
    fn reconstruct_skips_expired_and_fired_oneshot_loops() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("resources_state.json"),
            r#"{
              "state": {
                "distill.Scheduler": {
                  "tasks": [
                    {"id":"live","intervalSecs":30,"prompt":"ping"},
                    {"id":"expired","intervalSecs":30,"prompt":"old","expiresAt":"2000-01-01T00:00:00Z"},
                    {"id":"oneshot","intervalSecs":30,"prompt":"once","recurring":false,"lastFiredAt":"2026-01-01T00:00:00Z"}
                  ]
                }
              }
            }"#,
        )
        .unwrap();
        let snap = reconstruct_from_disk(dir.path(), "sess", [], None);
        assert_eq!(
            snap.loops.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["live"]
        );
    }
}
