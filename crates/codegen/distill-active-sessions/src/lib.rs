// Modified for Distill by Samuel Fajreldines, 2026.
//! Tracks open TUI sessions in `~/.grok/active_sessions.json` for crash recovery.
//! A clean exit removes the entry; a crash leaves it behind.
//! On next launch, [`collect_crashed`] finds orphaned entries (dead PIDs).
//!
//! Entries also carry optional roster fields (repo, branch, objective, edited files, heartbeat)
//! so concurrent sessions in one repository can see each other; see [`list_live_in`].

#![deny(clippy::indexing_slicing)]

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

use agent_client_protocol as acp;
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveSession {
    pub session_id: acp::SessionId,
    pub pid: u32,
    pub cwd: String,
    pub opened_at: DateTime<Utc>,
    /// Git common dir, shared by every worktree of one repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// First user prompt, at most [`MAX_OBJECTIVE_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    /// Most recently edited files, oldest first, at most [`MAX_EDITING_FILES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editing_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<DateTime<Utc>>,
}

pub const MAX_OBJECTIVE_CHARS: usize = 160;
pub const MAX_EDITING_FILES: usize = 20;
/// An entry whose last heartbeat is older than this is not a live peer.
pub const HEARTBEAT_TTL: Duration = Duration::minutes(10);

impl ActiveSession {
    pub fn new(session_id: acp::SessionId, pid: u32, cwd: String) -> Self {
        Self {
            session_id,
            pid,
            cwd,
            opened_at: Utc::now(),
            repo_root: None,
            branch: None,
            objective: None,
            editing_files: Vec::new(),
            heartbeat_at: None,
        }
    }

    /// Last sign of life; entries written before heartbeats existed fall back to `opened_at`.
    pub fn last_seen(&self) -> DateTime<Utc> {
        self.heartbeat_at.unwrap_or(self.opened_at)
    }

    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        now.signed_duration_since(self.last_seen()) <= HEARTBEAT_TTL && is_pid_alive(self.pid)
    }

    /// Sets the objective from the first prompt only; whitespace is collapsed and the text capped.
    pub fn set_objective_if_unset(&mut self, prompt: &str) {
        if self.objective.is_some() {
            return;
        }
        let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            self.objective = Some(collapsed.chars().take(MAX_OBJECTIVE_CHARS).collect());
        }
    }

    /// Moves each path to the most-recent end, keeps the newest [`MAX_EDITING_FILES`], and beats the heart.
    pub fn record_edits(&mut self, paths: &[String], now: DateTime<Utc>) {
        for path in paths {
            self.editing_files.retain(|p| p != path);
            self.editing_files.push(path.clone());
        }
        let excess = self.editing_files.len().saturating_sub(MAX_EDITING_FILES);
        self.editing_files.drain(..excess);
        self.heartbeat_at = Some(now);
    }

    /// Fills roster fields this entry lacks from `prev`, so a re-registration keeps what was learned.
    fn inherit_roster(&mut self, prev: &ActiveSession) {
        self.repo_root = self.repo_root.take().or_else(|| prev.repo_root.clone());
        self.branch = self.branch.take().or_else(|| prev.branch.clone());
        self.objective = self.objective.take().or_else(|| prev.objective.clone());
        if self.editing_files.is_empty() {
            self.editing_files = prev.editing_files.clone();
        }
        self.heartbeat_at = self.heartbeat_at.or(prev.heartbeat_at);
    }
}

const DATA_FILENAME: &str = "active_sessions.json";
const LOCK_FILENAME: &str = "active_sessions.lock";
const TMP_FILENAME: &str = "active_sessions.json.tmp";

// -- Public API (delegates to `_in` variants with default grok home) --------

/// Register a session as active (idempotent by session_id).
pub fn register(session: ActiveSession) -> io::Result<()> {
    register_in(&distill_config::distill_home(), session)
}

/// Apply `update` to the entry for `session_id`; `Ok(false)` when it is not registered.
pub fn update(
    session_id: &acp::SessionId,
    update: impl FnOnce(&mut ActiveSession),
) -> io::Result<bool> {
    update_in(&distill_config::distill_home(), session_id, update)
}

/// Entries that are alive and fresh (see [`ActiveSession::is_live`]).
pub fn list_live() -> io::Result<Vec<ActiveSession>> {
    list_live_in(&distill_config::distill_home(), Utc::now())
}

/// Non-blocking unregister for signal handlers.
/// Returns `Ok(false)` on lock contention; the orphan is cleaned up by `collect_crashed` next launch.
pub fn try_unregister(session_id: &acp::SessionId) -> io::Result<bool> {
    try_unregister_in(&distill_config::distill_home(), session_id)
}

/// Remove entries with dead PIDs and return them.
pub fn collect_crashed() -> io::Result<Vec<ActiveSession>> {
    collect_crashed_in(&distill_config::distill_home())
}

// -- Injectable-root variants (`_in`) for testing ---------------------------

pub fn register_in(root: &Path, session: ActiveSession) -> io::Result<()> {
    with_locked_state(root, |sessions| {
        let mut session = session;
        if let Some(prev) = sessions.iter().find(|s| s.session_id == session.session_id) {
            session.inherit_roster(prev);
        }
        sessions.retain(|s| s.session_id != session.session_id);
        sessions.push(session);
    })
}

pub fn update_in(
    root: &Path,
    session_id: &acp::SessionId,
    update: impl FnOnce(&mut ActiveSession),
) -> io::Result<bool> {
    with_locked_state(root, |sessions| {
        match sessions.iter_mut().find(|s| s.session_id == *session_id) {
            Some(entry) => {
                update(entry);
                true
            }
            None => false,
        }
    })
}

pub fn list_live_in(root: &Path, now: DateTime<Utc>) -> io::Result<Vec<ActiveSession>> {
    let mut sessions = list_in(root)?;
    sessions.retain(|s| s.is_live(now));
    Ok(sessions)
}

pub fn unregister_in(root: &Path, session_id: &acp::SessionId) -> io::Result<()> {
    with_locked_state(root, |sessions| {
        sessions.retain(|s| s.session_id != *session_id);
    })
}

pub fn try_unregister_in(root: &Path, session_id: &acp::SessionId) -> io::Result<bool> {
    try_with_locked_state(root, |sessions| {
        sessions.retain(|s| s.session_id != *session_id);
    })
    .map(|opt| opt.is_some())
}

pub fn collect_crashed_in(root: &Path) -> io::Result<Vec<ActiveSession>> {
    with_locked_state(root, |sessions| {
        let (alive, dead): (Vec<_>, Vec<_>) = sessions.drain(..).partition(|s| is_pid_alive(s.pid));
        *sessions = alive;
        dead
    })
}

pub fn list_in(root: &Path) -> io::Result<Vec<ActiveSession>> {
    let data_path = root.join(DATA_FILENAME);
    read_data_file(&data_path)
}

// -- Internal: locked read-modify-write -------------------------------------

fn with_locked_state<F, R>(root: &Path, mutate: F) -> io::Result<R>
where
    F: FnOnce(&mut Vec<ActiveSession>) -> R,
{
    let lock_path = root.join(LOCK_FILENAME);
    let data_path = root.join(DATA_FILENAME);
    let tmp_path = root.join(TMP_FILENAME);

    fs::create_dir_all(root)?;
    let lock_file = open_lock_file(&lock_path)?;
    lock_file.lock_exclusive()?;

    let result = locked_mutate(&data_path, &tmp_path, mutate);

    let _ = lock_file.unlock();
    result
}

/// Non-blocking variant for signal handlers.
fn try_with_locked_state<F, R>(root: &Path, mutate: F) -> io::Result<Option<R>>
where
    F: FnOnce(&mut Vec<ActiveSession>) -> R,
{
    let lock_path = root.join(LOCK_FILENAME);
    let data_path = root.join(DATA_FILENAME);
    let tmp_path = root.join(TMP_FILENAME);

    fs::create_dir_all(root)?;
    let lock_file = open_lock_file(&lock_path)?;

    match lock_file.try_lock_exclusive() {
        Ok(()) => {
            let result = locked_mutate(&data_path, &tmp_path, mutate);
            let _ = lock_file.unlock();
            result.map(Some)
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

fn locked_mutate<F, R>(data_path: &Path, tmp_path: &Path, mutate: F) -> io::Result<R>
where
    F: FnOnce(&mut Vec<ActiveSession>) -> R,
{
    let mut sessions = read_data_file(data_path)?;
    let result = mutate(&mut sessions);
    write_data_file_atomic(tmp_path, data_path, &sessions)?;
    Ok(result)
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn read_data_file(path: &Path) -> io::Result<Vec<ActiveSession>> {
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(Vec::new()),
        Ok(bytes) => match serde_json::from_slice::<Vec<ActiveSession>>(&bytes) {
            Ok(sessions) => Ok(sessions),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "active_sessions.json is corrupted, starting with empty list"
                );
                Ok(Vec::new())
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn write_data_file_atomic(
    tmp_path: &Path,
    data_path: &Path,
    sessions: &[ActiveSession],
) -> io::Result<()> {
    let json = serde_json::to_string_pretty(sessions)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(tmp_path, json.as_bytes())?;
    fs::rename(tmp_path, data_path).inspect_err(|_| {
        let _ = fs::remove_file(tmp_path);
    })
}

fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let pid_i = match i32::try_from(pid) {
            Ok(p) if p > 0 => p,
            _ => return false,
        };
        let ret = unsafe { libc::kill(pid_i as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
        match handle {
            Ok(h) => {
                let _ = unsafe { CloseHandle(h) };
                true
            }
            Err(_) => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Conservative: assume alive if we can't check.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_session(id: &str, pid: u32) -> ActiveSession {
        ActiveSession::new(acp::SessionId::new(id), pid, "/tmp/test".into())
    }

    #[test]
    fn register_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let s = make_session("s1", std::process::id());
        register_in(dir.path(), s.clone()).unwrap();
        register_in(dir.path(), s).unwrap();
        assert_eq!(list_in(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn collect_crashed_partitions_by_pid_liveness() {
        let dir = TempDir::new().unwrap();
        register_in(dir.path(), make_session("alive", std::process::id())).unwrap();
        register_in(dir.path(), make_session("dead", 2_000_000_000)).unwrap();

        let crashed = collect_crashed_in(dir.path()).unwrap();
        assert_eq!(crashed.len(), 1);
        assert_eq!(
            crashed.first().map(|s| s.session_id.0.as_ref()),
            Some("dead")
        );
        assert_eq!(
            list_in(dir.path())
                .unwrap()
                .first()
                .map(|s| s.session_id.0.as_ref()),
            Some("alive")
        );
    }

    #[test]
    fn concurrent_registers_no_corruption() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        std::thread::scope(|s| {
            for i in 0..10 {
                let p = path.clone();
                s.spawn(move || {
                    register_in(&p, make_session(&format!("s{i}"), std::process::id())).unwrap()
                });
            }
        });
        assert_eq!(list_in(dir.path()).unwrap().len(), 10);
    }

    #[test]
    fn try_unregister_skips_if_locked() {
        let dir = TempDir::new().unwrap();
        let s = make_session("s1", std::process::id());
        register_in(dir.path(), s.clone()).unwrap();

        let lock_file = open_lock_file(&dir.path().join(LOCK_FILENAME)).unwrap();
        lock_file.lock_exclusive().unwrap();
        assert!(!try_unregister_in(dir.path(), &s.session_id).unwrap());
        lock_file.unlock().unwrap();
        assert_eq!(list_in(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_file_recovers() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(DATA_FILENAME), "garbage{{{").unwrap();
        assert!(list_in(dir.path()).unwrap().is_empty());
        register_in(dir.path(), make_session("s1", std::process::id())).unwrap();
        assert_eq!(list_in(dir.path()).unwrap().len(), 1);
    }

    fn old_format_json(id: &str, pid: u32) -> String {
        format!(
            r#"[{{"session_id":"{id}","pid":{pid},"cwd":"/tmp/old","opened_at":"{}"}}]"#,
            Utc::now().to_rfc3339()
        )
    }

    #[test]
    fn old_format_entry_reads_and_new_fields_default() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(DATA_FILENAME),
            old_format_json("old", std::process::id()),
        )
        .unwrap();
        let sessions = list_in(dir.path()).unwrap();
        let old = sessions.first().unwrap();
        assert_eq!(old.cwd, "/tmp/old");
        assert!(old.repo_root.is_none() && old.objective.is_none() && old.heartbeat_at.is_none());
        assert!(old.editing_files.is_empty());
    }

    #[test]
    fn old_entry_survives_a_rewrite_without_gaining_fields() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(DATA_FILENAME),
            old_format_json("old", std::process::id()),
        )
        .unwrap();
        register_in(dir.path(), make_session("new", std::process::id())).unwrap();
        let raw = fs::read_to_string(dir.path().join(DATA_FILENAME)).unwrap();
        assert!(!raw.contains("repo_root") && !raw.contains("editing_files"));
        assert_eq!(list_in(dir.path()).unwrap().len(), 2);
    }

    #[test]
    fn roster_fields_round_trip() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        let mut s = make_session("s1", std::process::id());
        s.repo_root = Some("/repo/.git".into());
        s.branch = Some("main".into());
        s.set_objective_if_unset("fix   the\nlogin bug");
        s.record_edits(&["/repo/a.rs".into()], now);
        register_in(dir.path(), s.clone()).unwrap();
        assert_eq!(list_in(dir.path()).unwrap(), vec![s.clone()]);
        assert_eq!(s.objective.as_deref(), Some("fix the login bug"));
    }

    #[test]
    fn re_registering_keeps_roster_fields_learned_earlier() {
        let dir = TempDir::new().unwrap();
        let mut first = make_session("s1", std::process::id());
        first.repo_root = Some("/repo/.git".into());
        first.set_objective_if_unset("goal");
        register_in(dir.path(), first).unwrap();
        register_in(dir.path(), make_session("s1", std::process::id())).unwrap();
        let sessions = list_in(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        let entry = sessions.first().unwrap();
        assert_eq!(entry.repo_root.as_deref(), Some("/repo/.git"));
        assert_eq!(entry.objective.as_deref(), Some("goal"));
    }

    #[test]
    fn objective_is_first_prompt_only_and_capped() {
        let mut s = make_session("s1", 1);
        s.set_objective_if_unset(&"x".repeat(500));
        s.set_objective_if_unset("second");
        assert_eq!(
            s.objective.as_ref().map(|o| o.chars().count()),
            Some(MAX_OBJECTIVE_CHARS)
        );
        assert!(s.objective.as_ref().is_some_and(|o| o.starts_with('x')));
    }

    #[test]
    fn editing_files_keep_newest_twenty_and_move_repeats_to_the_end() {
        let mut s = make_session("s1", 1);
        let now = Utc::now();
        let all: Vec<String> = (0..25).map(|i| format!("/f{i}")).collect();
        s.record_edits(&all, now);
        assert_eq!(s.editing_files.len(), MAX_EDITING_FILES);
        assert_eq!(s.editing_files.first().map(String::as_str), Some("/f5"));
        s.record_edits(&["/f10".to_string()], now);
        assert_eq!(s.editing_files.last().map(String::as_str), Some("/f10"));
        assert_eq!(s.editing_files.iter().filter(|p| *p == "/f10").count(), 1);
        assert_eq!(s.editing_files.len(), MAX_EDITING_FILES);
    }

    #[test]
    fn update_in_changes_only_the_named_entry() {
        let dir = TempDir::new().unwrap();
        register_in(dir.path(), make_session("a", std::process::id())).unwrap();
        register_in(dir.path(), make_session("b", std::process::id())).unwrap();
        let now = Utc::now();
        let hit = update_in(dir.path(), &acp::SessionId::new("a"), |s| {
            s.record_edits(&["/x".into()], now)
        })
        .unwrap();
        assert!(hit);
        assert!(!update_in(dir.path(), &acp::SessionId::new("zzz"), |_| {}).unwrap());
        let sessions = list_in(dir.path()).unwrap();
        let by = |id: &str| sessions.iter().find(|s| &*s.session_id.0 == id).unwrap();
        assert_eq!(by("a").editing_files, vec!["/x".to_string()]);
        assert!(by("b").editing_files.is_empty());
    }

    #[test]
    fn list_live_drops_dead_pids_and_stale_heartbeats() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        let pid = std::process::id();
        let mut fresh = make_session("fresh", pid);
        fresh.heartbeat_at = Some(now - Duration::minutes(9));
        let mut stale = make_session("stale", pid);
        stale.heartbeat_at = Some(now - Duration::minutes(11));
        let mut dead = make_session("dead", 2_000_000_000);
        dead.heartbeat_at = Some(now);
        let mut old_no_heartbeat = make_session("old", pid);
        old_no_heartbeat.opened_at = now - Duration::minutes(30);
        for s in [fresh, stale, dead, old_no_heartbeat] {
            register_in(dir.path(), s).unwrap();
        }
        let live = list_live_in(dir.path(), now).unwrap();
        let ids: Vec<&str> = live.iter().map(|s| &*s.session_id.0).collect();
        assert_eq!(ids, vec!["fresh"]);
    }
}
