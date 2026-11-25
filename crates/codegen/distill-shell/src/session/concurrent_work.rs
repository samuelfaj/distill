// Modified for Distill by Samuel Fajreldines, 2026.
//! Session roster for runs outside MacCode: publishes this session's repo, branch, objective and
//! edited files in the shared active-session registry, and renders the `<concurrent_work>` reminder
//! listing other live sessions in the same repository.
//! MacCode hosts its own version of this block, so the reminder is skipped when it hosts the session.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use chrono::{DateTime, Utc};
use distill_active_sessions::ActiveSession;
use parking_lot::Mutex;

use crate::util::text_sanitize::flatten_to_spaces;

pub(crate) const REMINDER_TAG: &str = "concurrent_work";
const MACCODE_HOST_ENV: &str = "RC_REMOTE_CODE_AGENT_APP_SOCKET_PATH";
const EDIT_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_PEERS: usize = 5;
const MAX_FILES_SHOWN: usize = 10;

static ROSTERS: LazyLock<Mutex<HashMap<String, Arc<Roster>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn is_hosted(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// Tests run unhosted whatever the developer's shell exports.
fn hosted_by_maccode() -> bool {
    !cfg!(test) && is_hosted(std::env::var_os(MACCODE_HOST_ENV))
}

#[derive(Default)]
struct EditThrottle {
    pending: Vec<String>,
    last_flush: Option<Instant>,
    trailing_scheduled: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum EditDecision {
    FlushNow(Vec<String>),
    ScheduleTrailing,
    Queued,
}

/// This session's entry in the registry plus the edit batching in front of it.
pub(crate) struct Roster {
    root: PathBuf,
    base: ActiveSession,
    throttle: Mutex<EditThrottle>,
    closed: AtomicBool,
}

/// Unregisters the session when dropped.
pub(crate) struct RosterGuard(Arc<Roster>);

impl Drop for RosterGuard {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::SeqCst);
        let mut rosters = ROSTERS.lock();
        let id = self.0.base.session_id.0.to_string();
        if rosters.get(&id).is_some_and(|r| Arc::ptr_eq(r, &self.0)) {
            rosters.remove(&id);
        }
        drop(rosters);
        // Non-blocking so a slow registry file cannot hang exit; a leftover entry ages out by heartbeat.
        let _ = distill_active_sessions::try_unregister_in(&self.0.root, &self.0.base.session_id);
    }
}

/// Git common dir (shared by all worktrees of a repository) and current branch.
fn repo_identity(cwd: &Path) -> (Option<String>, Option<String>) {
    let Ok(repo) = git2::Repository::discover(cwd) else {
        return (None, None);
    };
    let common = repo.commondir();
    let root = std::fs::canonicalize(common).unwrap_or_else(|_| common.to_path_buf());
    let branch = repo
        .head()
        .ok()
        .filter(git2::Reference::is_branch)
        .and_then(|head| head.shorthand().map(str::to_owned));
    (Some(root.display().to_string()), branch)
}

/// Absolute paths of the files an edit, delete or move tool call touches.
pub(crate) fn edited_paths(tool_call: &acp::ToolCall, cwd: &Path) -> Vec<String> {
    if !matches!(
        tool_call.kind,
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move
    ) {
        return Vec::new();
    }
    let mut paths: Vec<String> = tool_call
        .locations
        .iter()
        .filter(|loc| !loc.path.as_os_str().is_empty())
        .map(|loc| cwd.join(&loc.path).display().to_string())
        .collect();
    // `apply_patch` has no locations; its file headers are in the patch text.
    if let Some(patch) = tool_call
        .raw_input
        .as_ref()
        .and_then(|input| input.get("patch"))
        .and_then(|patch| patch.as_str())
    {
        const HEADERS: [&str; 4] = [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ];
        paths.extend(patch.lines().filter_map(|line| {
            let rest = HEADERS.iter().find_map(|h| line.strip_prefix(h))?;
            Some(cwd.join(rest.trim()).display().to_string())
        }));
    }
    paths
}

impl Roster {
    fn new(
        root: PathBuf,
        session_id: acp::SessionId,
        cwd: &Path,
        repo_root: Option<String>,
        branch: Option<String>,
    ) -> Self {
        let mut base =
            ActiveSession::new(session_id, std::process::id(), cwd.display().to_string());
        base.repo_root = repo_root;
        base.branch = branch;
        Self {
            root,
            base,
            throttle: Mutex::new(EditThrottle::default()),
            closed: AtomicBool::new(false),
        }
    }

    fn note_edits(&self, paths: Vec<String>, now: Instant) -> EditDecision {
        let mut t = self.throttle.lock();
        for path in paths {
            t.pending.retain(|p| *p != path);
            t.pending.push(path);
        }
        if t.last_flush
            .is_none_or(|last| now.duration_since(last) >= EDIT_FLUSH_INTERVAL)
        {
            t.last_flush = Some(now);
            return EditDecision::FlushNow(std::mem::take(&mut t.pending));
        }
        if t.trailing_scheduled || t.pending.is_empty() {
            EditDecision::Queued
        } else {
            t.trailing_scheduled = true;
            EditDecision::ScheduleTrailing
        }
    }

    /// Drains queued edits regardless of the interval (turn start, trailing flush).
    fn take_pending(&self, now: Instant) -> Vec<String> {
        let mut t = self.throttle.lock();
        t.last_flush = Some(now);
        t.trailing_scheduled = false;
        std::mem::take(&mut t.pending)
    }

    /// Applies `change` to this session's entry, re-registering it first if something removed it.
    fn write(&self, change: impl Fn(&mut ActiveSession)) -> io::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let id = &self.base.session_id;
        if !distill_active_sessions::update_in(&self.root, id, &change)? {
            distill_active_sessions::register_in(&self.root, self.base.clone())?;
            distill_active_sessions::update_in(&self.root, id, &change)?;
        }
        Ok(())
    }

    fn write_edits(&self, files: &[String], at: DateTime<Utc>) -> io::Result<()> {
        self.write(|s| s.record_edits(files, at))
    }

    /// Refreshes the entry for a new turn and returns the reminder for it, if any.
    fn turn_start(&self, prompt: Option<&str>, now: DateTime<Utc>, hosted: bool) -> Option<String> {
        let pending = self.take_pending(Instant::now());
        let (_, branch) = repo_identity(Path::new(&self.base.cwd));
        let result = self.write(|s| {
            if let Some(prompt) = prompt {
                s.set_objective_if_unset(prompt);
            }
            if branch.is_some() {
                s.branch.clone_from(&branch);
            }
            s.record_edits(&pending, now);
        });
        if let Err(e) = result {
            tracing::warn!(error = %e, "concurrent work: failed to refresh session roster entry");
        }
        self.reminder(now, hosted)
    }

    fn reminder(&self, now: DateTime<Utc>, hosted: bool) -> Option<String> {
        if hosted {
            return None;
        }
        let repo = self.base.repo_root.as_deref()?;
        let mut peers: Vec<ActiveSession> = distill_active_sessions::list_live_in(&self.root, now)
            .ok()?
            .into_iter()
            .filter(|s| {
                s.session_id != self.base.session_id && s.repo_root.as_deref() == Some(repo)
            })
            .collect();
        peers.sort_by_key(|s| std::cmp::Reverse(s.last_seen()));
        peers.truncate(MAX_PEERS);
        render_reminder(&peers, now, Path::new(&self.base.cwd))
    }
}

fn clean(text: &str) -> String {
    flatten_to_spaces(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_age(secs: i64) -> String {
    match secs {
        ..60 => "under 1 min".to_string(),
        60..3600 => format!("{} min", secs / 60),
        _ => format!("{} h", secs / 3600),
    }
}

fn render_reminder(peers: &[ActiveSession], now: DateTime<Utc>, cwd: &Path) -> Option<String> {
    if peers.is_empty() {
        return None;
    }
    let mut out = String::from("Other Distill sessions are active in this repository:\n");
    for peer in peers {
        let objective = peer.objective.as_deref().map_or_else(
            || "no objective recorded".to_string(),
            |o| format!("\"{}\"", clean(o)),
        );
        let branch = peer
            .branch
            .as_deref()
            .map_or_else(|| "unknown".to_string(), clean);
        let age = format_age(now.signed_duration_since(peer.opened_at).num_seconds());
        out.push_str(&format!(
            "- {objective} (branch: {branch}, started {age} ago)"
        ));
        if peer.editing_files.is_empty() {
            out.push_str(", editing no files yet\n");
            continue;
        }
        let shown: Vec<String> = peer
            .editing_files
            .iter()
            .rev()
            .take(MAX_FILES_SHOWN)
            .map(|f| {
                let path = Path::new(f);
                clean(&path.strip_prefix(cwd).unwrap_or(path).display().to_string())
            })
            .collect();
        let hidden = peer.editing_files.len().saturating_sub(MAX_FILES_SHOWN);
        out.push_str(&format!(", editing: {}", shown.join(", ")));
        if hidden > 0 {
            out.push_str(&format!(" (+{hidden} more)"));
        }
        out.push('\n');
    }
    out.push_str(
        "Before editing a listed file, choose another file, wait, or use a worktree, and do not redo work another session is doing.",
    );
    Some(out)
}

/// Installs a roster over `root` for the session, as `register_session` does for the real registry.
#[cfg(test)]
pub(crate) fn install_for_test(
    root: &Path,
    session_id: &acp::SessionId,
    cwd: &Path,
    repo_root: &str,
) -> RosterGuard {
    let roster = Arc::new(Roster::new(
        root.to_path_buf(),
        session_id.clone(),
        cwd,
        Some(repo_root.to_owned()),
        Some("main".into()),
    ));
    distill_active_sessions::register_in(root, roster.base.clone()).unwrap();
    ROSTERS
        .lock()
        .insert(session_id.0.to_string(), roster.clone());
    RosterGuard(roster)
}

fn lookup(session_id: &acp::SessionId) -> Option<Arc<Roster>> {
    ROSTERS.lock().get(session_id.0.as_ref()).cloned()
}

/// Publishes the session in the registry; the returned guard unregisters it on drop.
pub(crate) async fn register_session(
    session_id: &acp::SessionId,
    cwd: &Path,
) -> Option<RosterGuard> {
    let session_id = session_id.clone();
    let cwd = cwd.to_path_buf();
    let roster = tokio::task::spawn_blocking(move || {
        let (repo_root, branch) = repo_identity(&cwd);
        let roster = Arc::new(Roster::new(
            distill_config::distill_home(),
            session_id,
            &cwd,
            repo_root,
            branch,
        ));
        distill_active_sessions::register_in(&roster.root, roster.base.clone())
            .inspect_err(
                |e| tracing::warn!(error = %e, "concurrent work: failed to register session"),
            )
            .ok()?;
        Some(roster)
    })
    .await
    .ok()??;
    ROSTERS
        .lock()
        .insert(roster.base.session_id.0.to_string(), roster.clone());
    Some(RosterGuard(roster))
}

fn spawn_flush(roster: Arc<Roster>, files: Vec<String>) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = roster.write_edits(&files, Utc::now()) {
            tracing::warn!(error = %e, "concurrent work: failed to record edited files");
        }
    });
}

/// Records edited files for the session, writing the registry at most once per interval.
/// An empty `paths` is a plain heartbeat, so a long turn without edits does not age out.
pub(crate) fn note_edits(session_id: &acp::SessionId, paths: Vec<String>) {
    let Some(roster) = lookup(session_id) else {
        return;
    };
    match roster.note_edits(paths, Instant::now()) {
        EditDecision::FlushNow(files) => spawn_flush(roster, files),
        EditDecision::ScheduleTrailing => {
            tokio::spawn(async move {
                tokio::time::sleep(EDIT_FLUSH_INTERVAL).await;
                let files = roster.take_pending(Instant::now());
                if !files.is_empty() {
                    spawn_flush(roster, files);
                }
            });
        }
        EditDecision::Queued => {}
    }
}

/// Refreshes the session's entry and returns the `<concurrent_work>` reminder body for this turn.
/// `prompt` is the user's message for human-initiated turns; the first one becomes the objective.
pub(crate) async fn turn_start(
    session_id: &acp::SessionId,
    prompt: Option<&str>,
) -> Option<String> {
    let roster = lookup(session_id)?;
    let prompt = prompt.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        roster.turn_start(prompt.as_deref(), Utc::now(), hosted_by_maccode())
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
#[path = "concurrent_work_tests.rs"]
mod tests;
