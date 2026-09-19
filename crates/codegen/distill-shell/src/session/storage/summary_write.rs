// Modified for Distill by Samuel Fajreldines, 2026.
//! Concurrency-safe, field-correct writes to a session's `summary.json`.
//!
//! The same `summary.json` is mutated by several writers and, on reconnect, by more than one persistence actor.
//! A whole-summary read-modify-write with no lock loses updates: a writer holding a stale read overwrites a concurrent writer's field on write-back.
//! That silently reverted `last_active_at` and `num_messages`, and the active session sank in the `/resume` picker.
//!
//! [`SummaryPatch`] expresses *intent* (a partial update) rather than a whole-struct snapshot.
//! [`apply_patch_locked`] applies it under an exclusive lock on a sidecar `summary.json.lock`.
//! The lock file is never renamed, so the lock spans the entire read-modify-write.
//! All writers funnel through it, so the read-modify-writes serialize across actors and processes.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use agent_client_protocol as acp;
use chrono::{DateTime, Utc};
use distill_sampling_types::ReasoningEffort;
use fs2::FileExt;

use crate::session::persistence::Summary;
use crate::session::worktree::WorktreeIdentity;

/// `Increment` is applied to the in-lock fresh read (never precomputed by the caller, which would re-open the race).
/// `Set` is an absolute rewrite (compaction / rewind).
#[derive(Debug, Clone)]
pub(crate) enum CounterOp {
    Increment(usize),
    Set(usize),
}

impl CounterOp {
    fn apply(&self, current: usize) -> usize {
        match self {
            CounterOp::Increment(n) => current.saturating_add(*n),
            CounterOp::Set(n) => *n,
        }
    }
}

/// Each `None` leaves the existing value unchanged (matches the legacy `update_current_model` behavior).
#[derive(Debug, Clone)]
pub(crate) struct ModelPatch {
    pub model_id: acp::ModelId,
    pub agent_name: Option<String>,
    pub reasoning_effort: Option<Option<ReasoningEffort>>,
}

/// `commit` and `branch` are last-writer-wins, including being cleared to `None`.
#[derive(Debug, Clone)]
pub(crate) struct GitHeadPatch {
    pub commit: Option<String>,
    pub branch: Option<String>,
}

/// `next_trace_turn` is monotonic.
/// `request_id` is applied only when this turn wins, so a stale lower-turn write cannot pair a high `next_trace_turn` with an older `request_id`.
#[derive(Debug, Clone)]
pub(crate) struct TraceTurnPatch {
    pub next_trace_turn: u64,
    pub request_id: Option<String>,
}

/// Only the set fields change; the rest are read fresh under the lock and preserved.
/// Per-field merge rules (see [`Summary::apply_patch`]): `last_active_at` / `next_trace_turn` / `chat_format_version` are monotonic (never lowered).
/// Counters apply to the fresh read; everything else is last-writer-wins on that field alone.
#[derive(Debug, Clone, Default)]
pub(crate) struct SummaryPatch {
    pub record_activity: bool,
    pub messages: Option<CounterOp>,
    pub chat_messages: Option<CounterOp>,
    pub chat_format_version: Option<u8>,
    pub trace_turn: Option<TraceTurnPatch>,
    pub attempt_id: Option<String>,
    pub model: Option<ModelPatch>,
    pub git_head: Option<GitHeadPatch>,
    pub collection_id: Option<String>,
    /// Set the session title unconditionally (last-writer-wins).
    /// Used by the manual `/rename` (`/title`) path, which must always win.
    /// Also marks the title manual (`Summary::title_is_manual`).
    pub generated_title: Option<String>,
    /// Set the session title only when the session has no title yet.
    /// Used by automatic LLM title generation so it never overwrites a title the user set via `/rename`.
    /// Ignored when `generated_title` is also set.
    pub generated_title_if_absent: Option<String>,
    /// Overwrite an existing *auto* title with a freshly regenerated one, but never a manual `/rename`.
    /// Used by the early-session title refresh (turns 3 and 6).
    /// Ignored when `generated_title` (manual) is also set.
    pub generated_title_regenerate: Option<String>,
    /// `/rename --auto`: clear the manual pin.
    /// A successful clear blanks `generated_title` *and* `session_summary` so `display_title()` is empty and if-absent can adopt again.
    /// A leftover pre-rename auto title would otherwise block regeneration.
    pub reset_title_to_auto: bool,
    pub cwd_switch_bookkeeping_generation: Option<u64>,
    /// Per-turn dashboard summary as `(text, prompt_id)`.
    /// Outer `Some` applies (last-writer-wins); `Some(None)` clears it (conversation rewind removed the described work).
    pub last_turn_summary: Option<Option<(String, String)>>,
    /// Latest session recap preview.
    /// Outer `Some` applies (last-writer-wins); `Some(None)` clears it (rewind removed the described turns).
    /// Persisted so session lists can show a recap when available.
    pub last_recap: Option<Option<String>>,
    /// Stamp `session_kind` only when the summary has none yet.
    /// Used by the session/new headless stamp; a kind already on disk (crash-recovered dir, concurrent writer) is never overwritten.
    pub session_kind_if_absent: Option<String>,
}

impl Summary {
    /// Apply `patch` in place using the per-field merge rules.
    /// `now` is the single timestamp used for both `last_active_at` (when activity is recorded) and `updated_at`.
    /// Returns `true` iff an auto title was adopted (`generated_title_if_absent` or `generated_title_regenerate`).
    pub(crate) fn apply_patch(&mut self, patch: &SummaryPatch, now: DateTime<Utc>) -> bool {
        if patch.record_activity {
            // Monotonic: a stale concurrent writer can never move it backwards.
            self.last_active_at = Some(
                self.last_active_at
                    .map_or(now, |existing| existing.max(now)),
            );
        }
        if let Some(op) = &patch.messages {
            self.num_messages = op.apply(self.num_messages);
        }
        if let Some(op) = &patch.chat_messages {
            self.num_chat_messages = op.apply(self.num_chat_messages);
        }
        if let Some(version) = patch.chat_format_version {
            self.chat_format_version = self.chat_format_version.max(version);
        }
        if let Some(generation) = patch.cwd_switch_bookkeeping_generation
            && generation > self.cwd_switch_bookkeeping_generation
        {
            self.cwd_switch_bookkeeping_generation = generation;
            // An explicit chat counter op already owns the resulting count (append increments; history replacement sets)
            // Without one, this patch repairs a line found on disk after an earlier summary failure
            if patch.chat_messages.is_none() {
                self.num_chat_messages = self.num_chat_messages.saturating_add(1);
            }
        }
        if let Some(trace_turn) = &patch.trace_turn {
            // next_trace_turn is monotonic; keep request_id paired with the winning turn so a stale lower-turn write can't re-pair them
            if trace_turn.next_trace_turn >= self.next_trace_turn {
                self.next_trace_turn = trace_turn.next_trace_turn;
                if let Some(request_id) = &trace_turn.request_id {
                    self.request_id = Some(request_id.clone());
                }
            }
        }
        if let Some(attempt_id) = &patch.attempt_id {
            self.attempt_id = Some(attempt_id.clone());
        }
        if let Some(model) = &patch.model {
            self.current_model_id = model.model_id.clone();
            if let Some(agent_name) = &model.agent_name {
                self.agent_name = Some(agent_name.clone());
            }
            if let Some(reasoning_effort) = &model.reasoning_effort {
                self.reasoning_effort = *reasoning_effort;
            }
        }
        if let Some(git_head) = &patch.git_head {
            self.head_commit = git_head.commit.clone();
            self.head_branch = git_head.branch.clone();
        }
        if let Some(collection_id) = &patch.collection_id {
            self.collection_id = Some(collection_id.clone());
        }
        if let Some(last_turn_summary) = &patch.last_turn_summary {
            let (text, prompt_id) = last_turn_summary.clone().unzip();
            self.last_turn_summary = text;
            self.last_turn_summary_prompt_id = prompt_id;
        }
        if let Some(recap) = &patch.last_recap {
            self.last_recap = recap.clone();
        }
        if let Some(kind) = &patch.session_kind_if_absent
            && self.session_kind.is_none()
        {
            self.session_kind = Some(kind.clone());
        }
        let mut absent_title_applied = false;
        if patch.reset_title_to_auto {
            // Gate on a real pin (`manual_title_opt`), not a stale flag over a blank `generated_title`
            // That would wipe a legitimate auto title living in `session_summary`
            let cleared_manual = self.manual_title_opt().is_some();
            if cleared_manual {
                self.generated_title = None;
                // Blank both fields so `display_title()` is empty and `set_generated_title_if_absent` can adopt again
                // A leftover pre-rename auto title in `session_summary` would otherwise pin display forever
                self.session_summary.clear();
            }
            self.title_is_manual = false;
            self.updated_at = now;
            return cleared_manual;
        } else if let Some(title) = &patch.generated_title {
            self.set_title(title);
            // Manual `/rename`: recorded so clients can restore the prompt-border title on resume
            self.title_is_manual = true;
        } else if let Some(title) = &patch.generated_title_regenerate {
            // Early-session refresh: replace an existing auto title, but never a manual `/rename` (checked atomically under the summary lock)
            if !self.title_is_manual {
                self.set_title_overwrite(title);
                absent_title_applied = true;
            }
        } else if let Some(title) = &patch.generated_title_if_absent {
            // Auto-generated titles defer to any title already present, so a manual `/rename` is never overwritten by a racing LLM title
            if self.display_title().trim().is_empty() {
                self.set_title(title);
                // Defensive: an adopted auto title is never manual.
                self.title_is_manual = false;
                absent_title_applied = true;
            }
        }
        self.updated_at = now;
        absent_title_applied
    }

    /// Set `generated_title`, mirroring into `session_summary` while that field is still empty.
    /// Older clients that only read `session_summary` then see the title too.
    fn set_title(&mut self, title: &str) {
        self.generated_title = Some(title.to_owned());
        if self.session_summary.is_empty() {
            self.session_summary = title.to_owned();
        }
    }

    /// Replace an auto title with a refreshed one, updating the mirrored `session_summary` too so older clients that only read it see the new title.
    /// Only called for non-manual titles, so no manual `/rename` is overwritten.
    fn set_title_overwrite(&mut self, title: &str) {
        self.generated_title = Some(title.to_owned());
        self.session_summary = title.to_owned();
    }
}

/// Both arms carry the lock-fresh on-disk summary, so a caller that adopts it always displays exactly what the file says.
pub(crate) enum WorktreeIdentityRepair {
    /// The lock-fresh read was missing identity the file now has.
    Applied(Summary),
    /// The lock-fresh read already had a kind and a label; nothing was written.
    AlreadyKinded(Summary),
}

impl WorktreeIdentityRepair {
    /// The lock-fresh on-disk summary, whichever way the repair went.
    pub(crate) fn into_summary(self) -> Summary {
        match self {
            Self::Applied(summary) | Self::AlreadyKinded(summary) => summary,
        }
    }
}

/// A kinded summary that is still missing `worktree_label` gets only the label.
/// Kind and source stay put, so a legacy fork is not rewritten as `worktree`.
/// A kinded and labeled summary is never rewritten.
pub(crate) fn repair_worktree_identity(
    summary_path: &Path,
    lock_path: &Path,
    identity: &WorktreeIdentity,
) -> io::Result<WorktreeIdentityRepair> {
    let lock = open_lock_file(lock_path)?;
    lock.lock_exclusive()?;
    let result = (|| {
        let mut summary = read_summary(summary_path)?;
        if summary.session_kind.is_none() {
            summary.stamp_worktree_identity(identity);
        } else if summary.worktree_label.is_none() {
            // Keep the existing kind: a fork on a worktree path is still a fork
            // Only the display label was dropped when merge stopped calling lookup_worktree_label
            summary.worktree_label = Some(identity.label.clone());
        } else {
            return Ok(WorktreeIdentityRepair::AlreadyKinded(summary));
        }
        // Deliberately not apply_patch: the stamp is metadata repair, not activity, so updated_at stays put.
        // Bumping it would reshuffle listings (it is the sort key when last_active_at is absent).
        // Restore the pre-write mtime so the candidate window still matches the unrepaired file A restore failure must not fail the heal: the stamp is already on disk and the caller would otherwise keep an untagged summary.
        let previous_mtime = std::fs::metadata(summary_path)
            .ok()
            .and_then(|meta| meta.modified().ok());
        write_summary_atomic(summary_path, &summary)?;
        if let Some(mtime) = previous_mtime
            && let Err(error) = restore_summary_mtime(summary_path, mtime)
        {
            tracing::warn!(
                "failed to restore summary mtime after worktree identity heal on {}: {error}",
                summary_path.display()
            );
        }
        Ok(WorktreeIdentityRepair::Applied(summary))
    })();
    let _ = lock.unlock();
    result
}

/// Read-site repair for a `summary` that is untagged, or kinded but still missing `worktree_label`, when its cwd is inside a grok-managed worktree.
/// Stamps the missing identity on disk via [`repair_worktree_identity`] and replaces `summary` with the lock-fresh on-disk state.
/// Memory then mirrors disk whichever way the repair went.
pub(crate) fn repair_untagged_worktree_summary(
    summary: &mut Summary,
    summary_path: &Path,
    lock_path: &Path,
) {
    if summary.session_kind.is_some() && summary.worktree_label.is_some() {
        return;
    }
    let Some(identity) = crate::session::worktree::worktree_identity_for_cwd(&summary.info.cwd)
    else {
        return;
    };
    match repair_worktree_identity(summary_path, lock_path, &identity) {
        Ok(repair) => *summary = repair.into_summary(),
        Err(error) => tracing::warn!(
            "failed to repair worktree identity onto {}: {error}",
            summary_path.display()
        ),
    }
}

/// Keep a parseable on-disk agent id and always mint a new attempt for this activation.
/// A required id rejects a different persisted identity and adopts the id when none is stored.
/// Synchronous: callers run it on `spawn_blocking` because lock acquisition blocks.
pub(crate) fn stamp_session_identity_if_absent(
    summary_path: &Path,
    lock_path: &Path,
    required_agent_id: Option<&str>,
    mint: impl FnOnce(Option<&str>) -> crate::session::persistence::SessionIdentity,
) -> io::Result<crate::session::persistence::SessionIdentity> {
    let lock = open_lock_file(lock_path)?;
    lock.lock_exclusive()?;
    let result = (|| {
        let mut summary = read_summary(summary_path)?;
        let mut live_agent_id = summary
            .agent_id
            .as_deref()
            .and_then(distill_message_delivery_core::AgentId::parse)
            .map(|id| id.as_str().to_owned());
        if let Some(required_agent_id) = required_agent_id {
            match live_agent_id.as_deref() {
                Some(live) if live != required_agent_id => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "persisted agent id does not match the requested agent",
                    ));
                }
                Some(_) => {}
                None => live_agent_id = Some(required_agent_id.to_owned()),
            }
        }
        let mut identity = mint(live_agent_id.as_deref());
        if let Some(agent_id) = live_agent_id {
            identity.agent_id = agent_id;
        }
        summary.agent_id = Some(identity.agent_id.clone());
        summary.attempt_id = Some(identity.attempt_id.clone());
        summary.updated_at = chrono::Utc::now();
        write_summary_atomic(summary_path, &summary)?;
        Ok(identity)
    })();
    let _ = lock.unlock();
    result
}

pub(crate) fn update_wake_start_locked(
    summary_path: &Path,
    lock_path: &Path,
    start: crate::session::persistence::WakeStart,
    abort: &tokio_util::sync::CancellationToken,
) -> io::Result<()> {
    let crate::session::persistence::WakeStart {
        prior,
        attempt_id,
        next_trace_turn,
        model_id,
        agent_name,
        reasoning_effort,
    } = start;
    let lock = open_lock_file(lock_path)?;
    lock.lock_exclusive()?;
    let result = (|| {
        if abort.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "wake aborted"));
        }
        let mut summary = read_summary(summary_path)?;
        if summary.attempt_id != prior.attempt_id
            || summary.next_trace_turn != prior.next_trace_turn
            || summary.current_model_id != prior.current_model_id
            || summary.agent_name != prior.agent_name
            || summary.reasoning_effort != prior.reasoning_effort
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wake summary changed before start publication",
            ));
        }
        summary.attempt_id = Some(attempt_id);
        summary.next_trace_turn = next_trace_turn;
        summary.current_model_id = model_id;
        if let Some(agent_name) = agent_name {
            summary.agent_name = Some(agent_name);
        }
        if let Some(reasoning_effort) = reasoning_effort {
            summary.reasoning_effort = reasoning_effort;
        }
        summary.updated_at = Utc::now();
        write_summary_atomic(summary_path, &summary)
    })();
    let _ = lock.unlock();
    result
}

pub(crate) fn restore_wake_summary_locked(
    summary_path: &Path,
    lock_path: &Path,
    prior: crate::session::persistence::WakeSummaryState,
) -> io::Result<()> {
    let lock = open_lock_file(lock_path)?;
    lock.lock_exclusive()?;
    let result = super::write_bytes_atomic(summary_path, &prior.summary_bytes);
    let _ = lock.unlock();
    result
}

pub(crate) fn apply_patch_locked(
    summary_path: &Path,
    lock_path: &Path,
    patch: &SummaryPatch,
) -> io::Result<bool> {
    let lock = open_lock_file(lock_path)?;
    lock.lock_exclusive()?;
    let result = read_modify_write(summary_path, patch);
    let _ = lock.unlock();
    result
}

fn read_modify_write(summary_path: &Path, patch: &SummaryPatch) -> io::Result<bool> {
    let mut summary = read_summary(summary_path)?;
    let absent_title_applied = summary.apply_patch(patch, Utc::now());
    write_summary_atomic(summary_path, &summary)?;
    Ok(absent_title_applied)
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn read_summary(path: &Path) -> io::Result<Summary> {
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("summary.json is empty (0 bytes): {}", path.display()),
        ));
    }
    serde_json::from_slice::<Summary>(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_summary_atomic(summary_path: &Path, summary: &Summary) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(summary)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    crate::session::storage::write_bytes_atomic(summary_path, &bytes)
}

#[cfg(test)]
thread_local! {
    static RESTORE_MTIME_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_restore_summary_mtime() {
    RESTORE_MTIME_FAULT.set(true);
}

fn restore_summary_mtime(path: &Path, mtime: std::time::SystemTime) -> io::Result<()> {
    #[cfg(test)]
    if RESTORE_MTIME_FAULT.replace(false) {
        return Err(io::Error::other("injected mtime restore failure"));
    }
    OpenOptions::new()
        .write(true)
        .open(path)?
        .set_modified(mtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::info::Info;
    use crate::session::storage::StorageAdapter;
    use crate::session::storage::jsonl::JsonlStorageAdapter;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    fn test_info() -> Info {
        Info {
            id: acp::SessionId::new("concurrent-summary-test"),
            cwd: "/test".into(),
        }
    }

    /// Regression guard for the `/resume` "frozen `last_active_at`" lost-update race.
    /// Without the sidecar lock the metadata writer reverts the appender's `num_messages` / `last_active_at` (and vice versa).
    /// The invariants below are exact, so a regression that drops the lock fails this deterministically.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_do_not_lose_updates() {
        const N: usize = 300;
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("session");
        let info = test_info();

        let init = JsonlStorageAdapter::with_explicit_session_dir(session_dir.clone());
        init.init_session(&info, acp::ModelId::new("test-model"))
            .await
            .unwrap();

        let appender = JsonlStorageAdapter::with_explicit_session_dir(session_dir.clone());
        let metadata = JsonlStorageAdapter::with_explicit_session_dir(session_dir.clone());
        let barrier = Arc::new(Barrier::new(2));

        let info_a = info.clone();
        let barrier_a = barrier.clone();
        let task_a = tokio::spawn(async move {
            barrier_a.wait().await;
            for _ in 0..N {
                appender
                    .apply_summary_patch(
                        &info_a,
                        SummaryPatch {
                            record_activity: true,
                            messages: Some(CounterOp::Increment(1)),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            }
        });

        let info_b = info.clone();
        let barrier_b = barrier.clone();
        let task_b = tokio::spawn(async move {
            barrier_b.wait().await;
            for turn in 0..N {
                metadata
                    .apply_summary_patch(
                        &info_b,
                        SummaryPatch {
                            trace_turn: Some(TraceTurnPatch {
                                next_trace_turn: turn as u64,
                                request_id: None,
                            }),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            }
        });

        task_a.await.unwrap();
        task_b.await.unwrap();

        let summary = read_summary(&session_dir.join("summary.json")).unwrap();
        assert_eq!(
            summary.num_messages, N,
            "lost an append increment to a racing metadata write",
        );
        assert_eq!(
            summary.next_trace_turn,
            (N - 1) as u64,
            "monotonic next_trace_turn regressed under contention",
        );
        assert!(
            summary.last_active_at.is_some(),
            "activity timestamp was lost",
        );
    }

    /// A freshly-initialized (untitled) session: returns its adapter and the path to the on-disk `summary.json`.
    async fn new_session(dir: &TempDir) -> (JsonlStorageAdapter, Info, std::path::PathBuf) {
        let session_dir = dir.path().join("session");
        let info = test_info();
        let adapter = JsonlStorageAdapter::with_explicit_session_dir(session_dir.clone());
        adapter
            .init_session(&info, acp::ModelId::new("test-model"))
            .await
            .unwrap();
        (adapter, info, session_dir.join("summary.json"))
    }

    #[tokio::test]
    async fn second_identity_stamp_keeps_agent_id_and_rotates_attempt_id() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        let first = adapter
            .stamp_session_identity_if_absent(&info, None, |_| {
                crate::session::persistence::SessionIdentity {
                    agent_id: "ag1.a1".to_owned(),
                    attempt_id: "at1.a1".to_owned(),
                }
            })
            .await
            .unwrap();
        assert_eq!(
            first,
            crate::session::persistence::SessionIdentity {
                agent_id: "ag1.a1".to_owned(),
                attempt_id: "at1.a1".to_owned(),
            }
        );

        let second = adapter
            .stamp_session_identity_if_absent(&info, None, |live_agent_id| {
                assert_eq!(live_agent_id, Some("ag1.a1"));
                crate::session::persistence::SessionIdentity {
                    agent_id: "ag1.a2".to_owned(),
                    attempt_id: "at1.a2".to_owned(),
                }
            })
            .await
            .unwrap();
        assert_eq!(
            second,
            crate::session::persistence::SessionIdentity {
                agent_id: "ag1.a1".to_owned(),
                attempt_id: "at1.a2".to_owned(),
            }
        );

        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.agent_id.as_deref(), Some("ag1.a1"));
        assert_eq!(summary.attempt_id.as_deref(), Some("at1.a2"));
    }

    #[tokio::test]
    async fn identity_stamp_adopts_required_uuid() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;
        let agent_id = "019f972a-7c1b-7d92-a896-4f08f91b6864";

        let stamped = adapter
            .stamp_session_identity_if_absent(
                &info,
                Some(agent_id.to_owned()),
                move |live_agent_id| {
                    assert_eq!(live_agent_id, Some(agent_id));
                    crate::session::persistence::SessionIdentity {
                        agent_id: live_agent_id.unwrap().to_owned(),
                        attempt_id: "at1.c1".to_owned(),
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(stamped.agent_id, agent_id);
        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.agent_id.as_deref(), Some(agent_id));
        assert_eq!(summary.attempt_id.as_deref(), Some("at1.c1"));
    }

    #[tokio::test]
    async fn identity_stamp_rejects_mismatched_required_uuid() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;
        let mut summary = read_summary(&summary_path).unwrap();
        summary.agent_id = Some("019f972a-7c1b-7d92-a896-4f08f91b6864".to_owned());
        write_summary_atomic(&summary_path, &summary).unwrap();

        let error = adapter
            .stamp_session_identity_if_absent(
                &info,
                Some("019f972a-7c1b-7d92-a896-4f08f91b6865".to_owned()),
                |_| unreachable!("mismatch must fail before minting"),
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// An unparseable on-disk agent id is treated as absent: the mint closure sees `None` and its agent replaces the bad token.
    #[tokio::test]
    async fn identity_stamp_replaces_unparseable_agent_id() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        let mut summary = read_summary(&summary_path).unwrap();
        summary.agent_id = Some("legacy-session-id".to_owned());
        write_summary_atomic(&summary_path, &summary).unwrap();

        let stamped = adapter
            .stamp_session_identity_if_absent(&info, None, |live_agent_id| {
                assert_eq!(live_agent_id, None);
                crate::session::persistence::SessionIdentity {
                    agent_id: "ag1.b1".to_owned(),
                    attempt_id: "at1.b1".to_owned(),
                }
            })
            .await
            .unwrap();
        assert_eq!(
            stamped,
            crate::session::persistence::SessionIdentity {
                agent_id: "ag1.b1".to_owned(),
                attempt_id: "at1.b1".to_owned(),
            }
        );

        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.agent_id.as_deref(), Some("ag1.b1"));
        assert_eq!(summary.attempt_id.as_deref(), Some("at1.b1"));
    }

    /// Auto title generation writes (and reports `true`) when the session has no title yet, mirroring into `session_summary` for old clients.
    #[tokio::test]
    async fn auto_title_applies_when_session_has_no_title() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        let applied = adapter
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap();

        assert!(applied);
        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.display_title(), "Auto Title");
        assert_eq!(summary.session_summary, "Auto Title");
        assert!(!summary.title_is_manual);
        assert!(summary.manual_title_opt().is_none());
    }

    /// Regression guard for the `/rename`-during-turn race: an auto-generated title that lands after a manual `/rename` must not overwrite it.
    /// It must also report `false` so callers skip the remote/registry sync.
    #[tokio::test]
    async fn auto_title_does_not_clobber_manual_rename() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .update_session_title(&info, "Manual Title".into())
            .await
            .unwrap();
        let applied = adapter
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap();

        assert!(!applied);
        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.display_title(), "Manual Title");
        assert_eq!(summary.manual_title_opt().as_deref(), Some("Manual Title"));
    }

    #[tokio::test]
    async fn manual_rename_overrides_existing_auto_title() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap();
        adapter
            .update_session_title(&info, "Manual Title".into())
            .await
            .unwrap();

        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.display_title(), "Manual Title");
        assert!(summary.title_is_manual, "manual rename must mark the title");
    }

    /// The race resolved under contention: a manual rename and an auto title race for the summary lock.
    /// The unconditional manual write wins if it lands last; the auto write defers if it lands last.
    /// It runs many iterations so a regression to an unconditional auto overwrite (or moving the "if absent" check outside the lock) fails reliably.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_manual_rename_always_wins_over_auto_title() {
        for _ in 0..100 {
            let dir = TempDir::new().unwrap();
            let (adapter, info, summary_path) = new_session(&dir).await;
            let barrier = Arc::new(Barrier::new(2));

            let manual = adapter.clone();
            let info_m = info.clone();
            let barrier_m = barrier.clone();
            let task_m = tokio::spawn(async move {
                barrier_m.wait().await;
                manual
                    .update_session_title(&info_m, "Manual Title".into())
                    .await
                    .unwrap();
            });

            let auto = adapter.clone();
            let info_a = info.clone();
            let barrier_a = barrier.clone();
            let task_a = tokio::spawn(async move {
                barrier_a.wait().await;
                auto.set_generated_title_if_absent(&info_a, "Auto Title".into())
                    .await
                    .unwrap();
            });

            task_m.await.unwrap();
            task_a.await.unwrap();

            let summary = read_summary(&summary_path).unwrap();
            assert_eq!(summary.display_title(), "Manual Title");
            // The manual flag survives the race in either landing order, so the prompt-border title is restored on resume
            assert!(summary.title_is_manual);
        }
    }

    /// `/rename --auto` after a manual rename that landed before any auto title: both generated_title and the mirrored session_summary are cleared.
    /// If-absent can then adopt again.
    #[tokio::test]
    async fn reset_title_to_auto_clears_manual_and_unmirrors_equal_summary() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .update_session_title(&info, "Manual Title".into())
            .await
            .unwrap();
        let applied = adapter
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap();
        assert!(!applied, "manual pin must block auto title");

        assert!(adapter.reset_title_to_auto(&info).await.unwrap());

        let summary = read_summary(&summary_path).unwrap();
        assert!(!summary.title_is_manual);
        assert!(summary.generated_title.is_none());
        assert!(
            summary.session_summary.is_empty(),
            "mirrored session_summary must be cleared so display_title is blank"
        );
        assert!(summary.display_title().trim().is_empty());
        assert!(summary.manual_title_opt().is_none());

        let applied = adapter
            .set_generated_title_if_absent(&info, "Fresh Auto".into())
            .await
            .unwrap();
        assert!(applied, "reset session must accept a subsequent auto title");
        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.display_title(), "Fresh Auto");
        assert!(!summary.title_is_manual);
    }

    /// Common path: auto title, then `/rename`, then `/rename --auto`.
    /// The leftover pre-rename auto title in `session_summary` must not block if-absent.
    #[tokio::test]
    async fn reset_after_auto_then_manual_blanks_display_and_accepts_if_absent() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap();
        adapter
            .update_session_title(&info, "Manual Title".into())
            .await
            .unwrap();
        let before = read_summary(&summary_path).unwrap();
        assert_eq!(before.session_summary, "Auto Title");
        assert_eq!(before.display_title(), "Manual Title");

        assert!(adapter.reset_title_to_auto(&info).await.unwrap());

        let summary = read_summary(&summary_path).unwrap();
        assert!(!summary.title_is_manual);
        assert!(summary.generated_title.is_none());
        assert!(
            summary.session_summary.is_empty(),
            "pre-rename auto leftover must be cleared so if-absent can fire"
        );
        assert!(
            summary.display_title().trim().is_empty(),
            "load/resume would mark_done() if display_title stayed non-blank"
        );

        let applied = adapter
            .set_generated_title_if_absent(&info, "Fresh Auto".into())
            .await
            .unwrap();
        assert!(
            applied,
            "auto→rename→unpin must accept a subsequent auto title"
        );
        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.display_title(), "Fresh Auto");
        assert!(!summary.title_is_manual);
    }

    /// Early-session title refresh overwrites an existing auto title, updating the mirrored `session_summary` so old clients see the new title too.
    #[tokio::test]
    async fn regenerate_overwrites_auto_title() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .set_generated_title_if_absent(&info, "First Prompt Title".into())
            .await
            .unwrap();
        let applied = adapter
            .regenerate_generated_title(&info, "Refined Real Topic".into())
            .await
            .unwrap();

        assert!(applied);
        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.display_title(), "Refined Real Topic");
        // The mirror is updated so `display_title()` (old clients) reflects the refresh
        assert_eq!(summary.session_summary, "Refined Real Topic");
        assert!(!summary.title_is_manual);
    }

    #[tokio::test]
    async fn regenerate_never_clobbers_manual_title() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .update_session_title(&info, "Manual Title".into())
            .await
            .unwrap();
        let applied = adapter
            .regenerate_generated_title(&info, "Auto Refresh".into())
            .await
            .unwrap();

        assert!(!applied);
        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.display_title(), "Manual Title");
        assert!(summary.title_is_manual);
    }

    /// A recap persists into `summary.json` (last-writer-wins) and clears on rewind, separate from the last-turn summary.
    #[tokio::test]
    async fn set_last_recap_persists_and_clears() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .set_last_recap(&info, Some("Where we left off: fixing the parser".into()))
            .await
            .unwrap();

        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(
            summary.last_recap.as_deref(),
            Some("Where we left off: fixing the parser")
        );
        // The recap is distinct from the last-turn summary
        assert!(summary.last_turn_summary.is_none());

        // A rewind clears it.
        adapter.set_last_recap(&info, None).await.unwrap();
        let summary = read_summary(&summary_path).unwrap();
        assert!(summary.last_recap.is_none());
    }

    /// Unpin on a never-renamed / auto-titled session is a no-op: the auto title stays, the flag stays false.
    #[tokio::test]
    async fn reset_title_to_auto_is_noop_when_not_manual() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        assert!(!adapter.reset_title_to_auto(&info).await.unwrap());
        let empty = read_summary(&summary_path).unwrap();
        assert!(!empty.title_is_manual);
        assert!(empty.generated_title.is_none());

        adapter
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap();
        assert!(!adapter.reset_title_to_auto(&info).await.unwrap());
        let summary = read_summary(&summary_path).unwrap();
        assert!(!summary.title_is_manual);
        assert_eq!(summary.display_title(), "Auto Title");
        assert_eq!(summary.session_summary, "Auto Title");
        assert!(
            !adapter
                .set_generated_title_if_absent(&info, "Fresh".into())
                .await
                .unwrap(),
            "no-op unpin must leave the auto title blocking if-absent"
        );
    }

    #[tokio::test]
    async fn session_kind_if_absent_stamps_unset_summary() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .set_session_kind_if_absent(&info, "headless".into())
            .await
            .unwrap();

        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(summary.session_kind.as_deref(), Some("headless"));
    }

    #[tokio::test]
    async fn session_kind_if_absent_preserves_existing_kind() {
        let dir = TempDir::new().unwrap();
        let (adapter, info, summary_path) = new_session(&dir).await;

        adapter
            .apply_summary_patch(
                &info,
                SummaryPatch {
                    session_kind_if_absent: Some("subagent".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        adapter
            .set_session_kind_if_absent(&info, "headless".into())
            .await
            .unwrap();

        let summary = read_summary(&summary_path).unwrap();
        assert_eq!(
            summary.session_kind.as_deref(),
            Some("subagent"),
            "an existing kind must win over a later stamp"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reset_and_auto_title_never_leave_manual_pin() {
        for _ in 0..50 {
            let dir = TempDir::new().unwrap();
            let (adapter, info, summary_path) = new_session(&dir).await;
            adapter
                .update_session_title(&info, "Manual Title".into())
                .await
                .unwrap();
            let barrier = Arc::new(Barrier::new(2));

            let reset = adapter.clone();
            let info_r = info.clone();
            let barrier_r = barrier.clone();
            let task_r = tokio::spawn(async move {
                barrier_r.wait().await;
                reset.reset_title_to_auto(&info_r).await.unwrap();
            });

            let auto = adapter.clone();
            let info_a = info.clone();
            let barrier_a = barrier.clone();
            let task_a = tokio::spawn(async move {
                barrier_a.wait().await;
                auto.set_generated_title_if_absent(&info_a, "Auto Title".into())
                    .await
                    .unwrap();
            });

            task_r.await.unwrap();
            task_a.await.unwrap();

            let summary = read_summary(&summary_path).unwrap();
            assert!(
                !summary.title_is_manual,
                "reset must clear the manual pin under contention"
            );
            assert_ne!(summary.display_title(), "Manual Title");
        }
    }
    fn as_json(summary: &Summary) -> serde_json::Value {
        serde_json::to_value(summary).unwrap()
    }

    fn identity(label: &str, source: Option<&str>) -> WorktreeIdentity {
        WorktreeIdentity {
            label: label.to_owned(),
            source_workspace_dir: source.map(str::to_owned),
        }
    }

    fn lock_path_for(summary_path: &Path) -> std::path::PathBuf {
        summary_path.with_file_name(format!("{}.lock", crate::session::storage::SUMMARY_FILE))
    }

    #[tokio::test]
    async fn repair_stamps_untagged_summary_on_disk_without_bumping_updated_at() {
        let dir = TempDir::new().unwrap();
        let (_adapter, _info, summary_path) = new_session(&dir).await;
        let mut expected = read_summary(&summary_path).unwrap();

        let repair = repair_worktree_identity(
            &summary_path,
            &lock_path_for(&summary_path),
            &identity("fix-bug", Some("/home/user/repo")),
        )
        .unwrap();

        let WorktreeIdentityRepair::Applied(applied) = repair else {
            panic!("expected Applied for an untagged summary");
        };
        expected.session_kind = Some("worktree".to_owned());
        expected.worktree_label = Some("fix-bug".to_owned());
        expected.source_workspace_dir = Some("/home/user/repo".to_owned());
        assert_eq!(as_json(&applied), as_json(&expected));
        assert_eq!(
            as_json(&read_summary(&summary_path).unwrap()),
            as_json(&applied),
            "returned summary must mirror the file",
        );
    }

    #[tokio::test]
    async fn repair_stamps_untagged_summary_without_refreshing_file_mtime() {
        let dir = TempDir::new().unwrap();
        let (_adapter, _info, summary_path) = new_session(&dir).await;
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&summary_path)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        let mtime_before = std::fs::metadata(&summary_path)
            .unwrap()
            .modified()
            .unwrap();

        let repair = repair_worktree_identity(
            &summary_path,
            &lock_path_for(&summary_path),
            &identity("fix-bug", Some("/home/user/repo")),
        )
        .unwrap();
        assert!(matches!(repair, WorktreeIdentityRepair::Applied(_)));

        let mtime_after = std::fs::metadata(&summary_path)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            mtime_after, mtime_before,
            "heal rewrite must not refresh mtime; list_sessions_recent uses it"
        );
        assert_eq!(
            read_summary(&summary_path).unwrap().session_kind.as_deref(),
            Some("worktree")
        );
    }

    #[tokio::test]
    async fn repair_keeps_applied_stamp_when_mtime_restore_fails() {
        let dir = TempDir::new().unwrap();
        let (_adapter, _info, summary_path) = new_session(&dir).await;
        fail_next_restore_summary_mtime();

        let repair = repair_worktree_identity(
            &summary_path,
            &lock_path_for(&summary_path),
            &identity("fix-bug", Some("/home/user/repo")),
        )
        .unwrap();
        let WorktreeIdentityRepair::Applied(applied) = repair else {
            panic!("mtime restore must not undo a stamp already on disk");
        };
        assert_eq!(applied.session_kind.as_deref(), Some("worktree"));
        assert_eq!(applied.worktree_label.as_deref(), Some("fix-bug"));
        assert_eq!(
            read_summary(&summary_path).unwrap().session_kind.as_deref(),
            Some("worktree")
        );
    }

    #[tokio::test]
    async fn repair_declines_already_kinded_labeled_summary_and_leaves_file_unwritten() {
        let dir = TempDir::new().unwrap();
        let (_adapter, _info, summary_path) = new_session(&dir).await;
        let mut on_disk = read_summary(&summary_path).unwrap();
        on_disk.session_kind = Some("fork".to_owned());
        on_disk.worktree_label = Some("existing".to_owned());
        std::fs::write(&summary_path, serde_json::to_vec_pretty(&on_disk).unwrap()).unwrap();
        let bytes_before = std::fs::read(&summary_path).unwrap();

        let repair = repair_worktree_identity(
            &summary_path,
            &lock_path_for(&summary_path),
            &identity("fix-bug", Some("/home/user/repo")),
        )
        .unwrap();

        let WorktreeIdentityRepair::AlreadyKinded(fresh) = repair else {
            panic!("expected AlreadyKinded for a kinded labeled summary");
        };
        assert_eq!(as_json(&fresh), as_json(&on_disk));
        assert_eq!(std::fs::read(&summary_path).unwrap(), bytes_before);
    }

    #[tokio::test]
    async fn repair_fills_missing_label_on_kinded_summary_without_changing_kind() {
        let dir = TempDir::new().unwrap();
        let (_adapter, _info, summary_path) = new_session(&dir).await;
        let mut on_disk = read_summary(&summary_path).unwrap();
        on_disk.session_kind = Some("fork".to_owned());
        on_disk.worktree_label = None;
        on_disk.source_workspace_dir = Some("/home/user/repo".to_owned());
        std::fs::write(&summary_path, serde_json::to_vec_pretty(&on_disk).unwrap()).unwrap();

        let repair = repair_worktree_identity(
            &summary_path,
            &lock_path_for(&summary_path),
            &identity("fix-bug", Some("/other/source")),
        )
        .unwrap();

        let WorktreeIdentityRepair::Applied(applied) = repair else {
            panic!("expected Applied for a kinded unlabeled summary");
        };
        assert_eq!(applied.session_kind.as_deref(), Some("fork"));
        assert_eq!(applied.worktree_label.as_deref(), Some("fix-bug"));
        assert_eq!(
            applied.source_workspace_dir.as_deref(),
            Some("/home/user/repo"),
            "existing source must not be overwritten by the path identity"
        );
        let fresh = read_summary(&summary_path).unwrap();
        assert_eq!(as_json(&fresh), as_json(&applied));
    }

    #[tokio::test]
    async fn repair_second_application_declines_and_keeps_first_stamp() {
        let dir = TempDir::new().unwrap();
        let (_adapter, _info, summary_path) = new_session(&dir).await;
        let lock_path = lock_path_for(&summary_path);
        repair_worktree_identity(
            &summary_path,
            &lock_path,
            &identity("fix-bug", Some("/home/user/repo")),
        )
        .unwrap();
        let bytes_after_first = std::fs::read(&summary_path).unwrap();

        let repair =
            repair_worktree_identity(&summary_path, &lock_path, &identity("other-label", None))
                .unwrap();

        let WorktreeIdentityRepair::AlreadyKinded(fresh) = repair else {
            panic!("expected the second repair to decline");
        };
        assert_eq!(fresh.worktree_label.as_deref(), Some("fix-bug"));
        assert_eq!(std::fs::read(&summary_path).unwrap(), bytes_after_first);
    }
}
