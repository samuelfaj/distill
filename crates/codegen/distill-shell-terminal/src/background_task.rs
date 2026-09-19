// Modified for Distill by Samuel Fajreldines, 2026.
//! Per-session registry the model queries for the status and output of commands launched with `is_background: true`.
//! `StreamingLocalTerminalRunner` does the spawning and streaming; this registry answers queries by task_id.
//! The in-memory `output` is truncated past `output_byte_limit`; `output_file` holds the full output on disk.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};

/// Task identifier (UUID string)
pub type TaskId = String;

/// A cloneable view of a task that can be returned to callers without holding locks.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    /// Unique task ID (UUID) given to the model
    pub task_id: TaskId,
    /// Internal tool_call_id for terminal registry lookup
    pub tool_call_id: String,
    pub command: String,
    pub cwd: String,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    /// In-memory output, truncated when it exceeds output_byte_limit
    pub output: String,
    /// Path to full output file on disk
    pub output_file: PathBuf,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    /// Whether task has completed (exited or was killed)
    pub completed: bool,
    /// Whether a blocking waiter has claimed this task.
    pub block_waited: bool,
    /// Whether this task was explicitly killed via the kill tool.
    pub explicitly_killed: bool,
}

impl TaskSnapshot {
    /// Returns seconds since start while the task runs, or total runtime once it completes.
    pub fn duration_secs(&self) -> f64 {
        let end = self.end_time.unwrap_or_else(Utc::now);
        (end - self.start_time).num_milliseconds() as f64 / 1000.0
    }
}

struct TaskEntry {
    snapshot: RwLock<TaskSnapshot>,
    exit_notify: Arc<Notify>,
}

/// Per-session registry for background tasks; each session has its own instance via `ToolContext.background_tasks`.
pub struct BackgroundTaskRegistry {
    tasks: Mutex<HashMap<TaskId, Arc<TaskEntry>>>,
    max_tasks: usize,
}

const DEFAULT_MAX_BACKGROUND_TASKS: usize = 10;

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            max_tasks: DEFAULT_MAX_BACKGROUND_TASKS,
        }
    }

    /// Create a registry with custom max tasks limit (for testing).
    pub fn with_max_tasks(max_tasks: usize) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            max_tasks,
        }
    }

    /// If at capacity, completed tasks are cleaned up first.
    /// Returns error if still at capacity after cleanup.
    pub async fn register(&self, snapshot: TaskSnapshot) -> Result<(), String> {
        let mut tasks = self.tasks.lock().await;

        // Cleanup completed tasks if at capacity
        if tasks.len() >= self.max_tasks {
            tasks.retain(|_, entry| {
                // Keep if not completed (check synchronously via try_read)
                entry
                    .snapshot
                    .try_read()
                    .map(|s| !s.completed)
                    .unwrap_or(true)
            });

            if tasks.len() >= self.max_tasks {
                return Err(format!(
                    "Maximum background tasks ({}) reached. Wait for tasks to complete or kill existing tasks.",
                    self.max_tasks
                ));
            }
        }

        let task_id = snapshot.task_id.clone();
        let entry = Arc::new(TaskEntry {
            snapshot: RwLock::new(snapshot),
            exit_notify: Arc::new(Notify::new()),
        });
        tasks.insert(task_id, entry);
        Ok(())
    }

    pub async fn get(&self, task_id: &str) -> Option<TaskSnapshot> {
        let tasks = self.tasks.lock().await;
        let entry = tasks.get(task_id)?;
        Some(entry.snapshot.read().await.clone())
    }

    /// Update task output (called by output collector after completion).
    pub async fn update_output(&self, task_id: &str, output: String, truncated: bool) {
        let tasks = self.tasks.lock().await;
        if let Some(entry) = tasks.get(task_id) {
            let mut snapshot = entry.snapshot.write().await;
            snapshot.output = output;
            snapshot.truncated = truncated;
        }
    }

    pub async fn mark_completed(
        &self,
        task_id: &str,
        exit_code: Option<i32>,
        signal: Option<String>,
    ) {
        let tasks = self.tasks.lock().await;
        if let Some(entry) = tasks.get(task_id) {
            {
                let mut snapshot = entry.snapshot.write().await;
                snapshot.completed = true;
                snapshot.exit_code = exit_code;
                snapshot.signal = signal;
                snapshot.end_time = Some(Utc::now());
            }
            entry.exit_notify.notify_waiters();
        }
    }

    /// Returns the task snapshot after completion or timeout.
    /// Returns `None` if task_id is not found.
    pub async fn wait_for_completion(
        &self,
        task_id: &str,
        timeout: Option<std::time::Duration>,
    ) -> Option<TaskSnapshot> {
        // Get entry without holding the lock during wait
        let entry = {
            let tasks = self.tasks.lock().await;
            tasks.get(task_id)?.clone()
        };

        // Register notification interest BEFORE checking completion
        // If mark_completed fires between the check and the wait, notify_waiters() wakes zero futures and the notification is permanently lost
        let notified = entry.exit_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        {
            let snapshot = entry.snapshot.read().await;
            if snapshot.completed {
                return Some(snapshot.clone());
            }
        }

        if let Some(timeout) = timeout {
            let _ = tokio::time::timeout(timeout, notified).await;
        } else {
            notified.await;
        }

        Some(entry.snapshot.read().await.clone())
    }

    pub async fn list(&self) -> Vec<TaskSnapshot> {
        let tasks = self.tasks.lock().await;
        let mut result = Vec::with_capacity(tasks.len());
        for entry in tasks.values() {
            result.push(entry.snapshot.read().await.clone());
        }
        result
    }

    pub async fn active_count(&self) -> usize {
        let tasks = self.tasks.lock().await;
        let mut count = 0;
        for entry in tasks.values() {
            if !entry.snapshot.read().await.completed {
                count += 1;
            }
        }
        count
    }
}

// ── Background task manifest for session resume ──

const MANIFEST_FILENAME: &str = "background_tasks_manifest.json";

/// Minimal snapshot of a running background task, persisted on session exit so that a resumed session can inform the model about orphaned tasks.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackgroundTaskManifestEntry {
    pub task_id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_command: Option<String>,
    pub output_file: PathBuf,
    pub start_time: std::time::SystemTime,
    pub cwd: String,
    #[serde(default)]
    pub kind: distill_tools::computer::types::TaskKind,
}

/// Only writes a file when `entries` is non-empty.
/// Session shutdown calls this when background tasks are intentionally left alive.
pub fn persist_manifest(session_dir: &Path, entries: Vec<BackgroundTaskManifestEntry>) {
    if entries.is_empty() {
        return;
    }
    let path = session_dir.join(MANIFEST_FILENAME);
    match serde_json::to_vec(&entries) {
        Ok(data) => {
            if let Err(e) = std::fs::write(&path, data) {
                tracing::warn!(%e, "failed to write background task manifest");
            }
        }
        Err(e) => {
            tracing::warn!(%e, "failed to serialize background task manifest");
        }
    }
}

/// Returns an empty vec if the manifest doesn't exist or can't be parsed.
pub fn load_and_clear_manifest(session_dir: &Path) -> Vec<BackgroundTaskManifestEntry> {
    let path = session_dir.join(MANIFEST_FILENAME);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    // Delete regardless of parse success; stale manifests should not accumulate
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&data).unwrap_or_default()
}

/// Format a system-reminder about background tasks that were running when the session was last active.
pub fn format_resumed_tasks_reminder(entries: &[BackgroundTaskManifestEntry]) -> String {
    use std::fmt::Write;

    let now = std::time::SystemTime::now();
    let mut buf = String::from(
        "This session was resumed. The following background tasks were running \
         when the session was last active and may still be in progress:\n",
    );
    for entry in entries {
        let cmd = entry.display_command.as_deref().unwrap_or(&entry.command);
        let ago = format_duration_ago(now, entry.start_time);
        let kind_label = match entry.kind {
            distill_tools::computer::types::TaskKind::Monitor => " [monitor]",
            distill_tools::computer::types::TaskKind::Bash => "",
        };
        let _ = writeln!(
            buf,
            "- \"{}\"{} (started {}): {}",
            entry.task_id, kind_label, ago, cmd
        );
        let _ = writeln!(buf, "  Output log: {}", entry.output_file.display());
    }
    buf.push_str(
        "Check whether each is still running and read its output log to determine \
         if it completed successfully.",
    );
    buf
}

fn format_duration_ago(now: std::time::SystemTime, start: std::time::SystemTime) -> String {
    let secs = now.duration_since(start).map(|d| d.as_secs()).unwrap_or(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            format!("{hours}h ago")
        } else {
            format!("{hours}h {mins}m ago")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_test_snapshot(task_id: &str) -> TaskSnapshot {
        TaskSnapshot {
            task_id: task_id.to_string(),
            tool_call_id: format!("tc-{}", task_id),
            command: "echo hello".to_string(),
            cwd: "/tmp".to_string(),
            start_time: Utc::now(),
            end_time: None,
            output: String::new(),
            output_file: PathBuf::from(format!("/tmp/{}.log", task_id)),
            truncated: false,
            exit_code: None,
            signal: None,
            completed: false,
            block_waited: false,
            explicitly_killed: false,
        }
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let registry = BackgroundTaskRegistry::new();
        let snapshot = make_test_snapshot("test-1");

        registry.register(snapshot).await.unwrap();

        let got = registry.get("test-1").await.unwrap();
        assert_eq!(got.task_id, "test-1");
        assert_eq!(got.command, "echo hello");
        assert!(!got.completed);
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let registry = BackgroundTaskRegistry::new();
        assert!(registry.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_update_output() {
        let registry = BackgroundTaskRegistry::new();
        registry
            .register(make_test_snapshot("test-1"))
            .await
            .unwrap();

        registry
            .update_output("test-1", "hello world".to_string(), false)
            .await;

        let got = registry.get("test-1").await.unwrap();
        assert_eq!(got.output, "hello world");
        assert!(!got.truncated);
    }

    #[tokio::test]
    async fn test_mark_completed() {
        let registry = BackgroundTaskRegistry::new();
        registry
            .register(make_test_snapshot("test-1"))
            .await
            .unwrap();

        registry.mark_completed("test-1", Some(0), None).await;

        let got = registry.get("test-1").await.unwrap();
        assert!(got.completed);
        assert_eq!(got.exit_code, Some(0));
        assert!(got.end_time.is_some());
    }

    #[tokio::test]
    async fn test_wait_for_completion_already_done() {
        let registry = BackgroundTaskRegistry::new();
        registry
            .register(make_test_snapshot("test-1"))
            .await
            .unwrap();
        registry.mark_completed("test-1", Some(0), None).await;

        let got = registry
            .wait_for_completion("test-1", Some(Duration::from_millis(100)))
            .await
            .unwrap();
        assert!(got.completed);
    }

    #[tokio::test]
    async fn test_wait_for_completion_with_timeout() {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        registry
            .register(make_test_snapshot("test-1"))
            .await
            .unwrap();

        let got = registry
            .wait_for_completion("test-1", Some(Duration::from_millis(50)))
            .await
            .unwrap();

        assert!(!got.completed);
    }

    #[tokio::test]
    async fn test_wait_for_completion_notified() {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        registry
            .register(make_test_snapshot("test-1"))
            .await
            .unwrap();

        let registry_clone = registry.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            registry_clone
                .mark_completed("test-1", Some(42), None)
                .await;
        });

        let got = registry
            .wait_for_completion("test-1", Some(Duration::from_secs(5)))
            .await
            .unwrap();

        assert!(got.completed);
        assert_eq!(got.exit_code, Some(42));
    }

    #[tokio::test]
    async fn test_max_tasks_limit() {
        let registry = BackgroundTaskRegistry::with_max_tasks(2);

        // Register up to limit
        registry
            .register(make_test_snapshot("task-1"))
            .await
            .unwrap();
        registry
            .register(make_test_snapshot("task-2"))
            .await
            .unwrap();

        let result = registry.register(make_test_snapshot("task-3")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Maximum background tasks"));
    }

    #[tokio::test]
    async fn test_max_tasks_cleanup_completed() {
        let registry = BackgroundTaskRegistry::with_max_tasks(2);

        registry
            .register(make_test_snapshot("task-1"))
            .await
            .unwrap();
        registry
            .register(make_test_snapshot("task-2"))
            .await
            .unwrap();

        registry.mark_completed("task-1", Some(0), None).await;

        registry
            .register(make_test_snapshot("task-3"))
            .await
            .unwrap();

        assert!(registry.get("task-1").await.is_none());
        assert!(registry.get("task-3").await.is_some());
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let registry = BackgroundTaskRegistry::new();
        registry
            .register(make_test_snapshot("task-1"))
            .await
            .unwrap();
        registry
            .register(make_test_snapshot("task-2"))
            .await
            .unwrap();

        let tasks = registry.list().await;
        assert_eq!(tasks.len(), 2);
    }

    #[tokio::test]
    async fn test_active_count() {
        let registry = BackgroundTaskRegistry::new();
        registry
            .register(make_test_snapshot("task-1"))
            .await
            .unwrap();
        registry
            .register(make_test_snapshot("task-2"))
            .await
            .unwrap();

        assert_eq!(registry.active_count().await, 2);

        registry.mark_completed("task-1", Some(0), None).await;
        assert_eq!(registry.active_count().await, 1);
    }

    // ── Manifest tests ──

    fn make_manifest_entry(task_id: &str, secs_ago: u64) -> BackgroundTaskManifestEntry {
        BackgroundTaskManifestEntry {
            task_id: task_id.to_string(),
            command: format!("rsync -aP src:{task_id} /data/"),
            display_command: None,
            output_file: PathBuf::from(format!("/tmp/sessions/tasks/{task_id}.log")),
            start_time: std::time::SystemTime::now() - Duration::from_secs(secs_ago),
            cwd: "/home/user".to_string(),
            kind: distill_tools::computer::types::TaskKind::Bash,
        }
    }

    #[test]
    fn manifest_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            make_manifest_entry("task-a", 3600),
            make_manifest_entry("task-b", 120),
        ];
        persist_manifest(dir.path(), entries);

        let loaded = load_and_clear_manifest(dir.path());
        let [a, b] = loaded.as_slice() else {
            panic!("expected two entries: {loaded:?}");
        };
        assert_eq!(a.task_id, "task-a");
        assert_eq!(b.task_id, "task-b");
        assert_eq!(a.command, "rsync -aP src:task-a /data/");

        // File is deleted after load
        let again = load_and_clear_manifest(dir.path());
        assert!(again.is_empty());
    }

    #[test]
    fn manifest_empty_entries_no_file() {
        let dir = tempfile::tempdir().unwrap();
        persist_manifest(dir.path(), Vec::new());
        assert!(!dir.path().join(MANIFEST_FILENAME).exists());
    }

    #[test]
    fn manifest_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_and_clear_manifest(dir.path());
        assert!(loaded.is_empty());
    }

    #[test]
    fn manifest_malformed_json_returns_empty_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILENAME);
        std::fs::write(&path, b"not valid json {{{").unwrap();

        let loaded = load_and_clear_manifest(dir.path());
        assert!(loaded.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn manifest_partial_json_missing_fields_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILENAME);
        // Valid JSON array but missing required fields
        std::fs::write(&path, br#"[{"task_id": "x"}]"#).unwrap();

        let loaded = load_and_clear_manifest(dir.path());
        assert!(loaded.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn format_reminder_single_task() {
        let entries = vec![make_manifest_entry("bg-1", 7200)];
        let reminder = format_resumed_tasks_reminder(&entries);
        assert!(reminder.contains("This session was resumed"));
        assert!(reminder.contains("bg-1"));
        assert!(reminder.contains("2h ago"));
        assert!(reminder.contains("rsync -aP src:bg-1 /data/"));
        assert!(reminder.contains("Output log:"));
        assert!(reminder.contains("Check whether each is still running"));
    }

    #[test]
    fn format_reminder_prefers_display_command() {
        let mut entry = make_manifest_entry("bg-1", 60);
        entry.display_command = Some("rsync /data".to_string());
        let reminder = format_resumed_tasks_reminder(&[entry]);
        assert!(reminder.contains("rsync /data"));
        assert!(!reminder.contains("rsync -aP"));
    }

    #[test]
    fn format_reminder_labels_monitor_tasks() {
        let mut entry = make_manifest_entry("mon-1", 300);
        entry.kind = distill_tools::computer::types::TaskKind::Monitor;
        let reminder = format_resumed_tasks_reminder(&[entry]);
        assert!(reminder.contains("[monitor]"));
    }

    #[test]
    fn format_reminder_no_label_for_bash_tasks() {
        let entry = make_manifest_entry("bg-1", 300);
        let reminder = format_resumed_tasks_reminder(&[entry]);
        assert!(!reminder.contains("[monitor]"));
    }

    #[test]
    fn manifest_roundtrip_preserves_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut entry = make_manifest_entry("mon-1", 60);
        entry.kind = distill_tools::computer::types::TaskKind::Monitor;
        persist_manifest(dir.path(), vec![entry]);

        let loaded = load_and_clear_manifest(dir.path());
        let [entry] = loaded.as_slice() else {
            panic!("expected 1 entry, got {}", loaded.len());
        };
        assert_eq!(
            entry.kind,
            distill_tools::computer::types::TaskKind::Monitor
        );
    }

    #[test]
    fn format_duration_seconds() {
        let now = std::time::SystemTime::now();
        assert_eq!(
            format_duration_ago(now, now - Duration::from_secs(30)),
            "30s ago"
        );
    }

    #[test]
    fn format_duration_minutes() {
        let now = std::time::SystemTime::now();
        assert_eq!(
            format_duration_ago(now, now - Duration::from_secs(300)),
            "5m ago"
        );
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        let now = std::time::SystemTime::now();
        assert_eq!(
            format_duration_ago(now, now - Duration::from_secs(5400)),
            "1h 30m ago"
        );
    }

    #[test]
    fn format_duration_exact_hours() {
        let now = std::time::SystemTime::now();
        assert_eq!(
            format_duration_ago(now, now - Duration::from_secs(7200)),
            "2h ago"
        );
    }

    #[test]
    fn format_duration_future_start_returns_zero() {
        let now = std::time::SystemTime::now();
        assert_eq!(
            format_duration_ago(now, now + Duration::from_secs(100)),
            "0s ago"
        );
    }
}
