// Modified for Distill by Samuel Fajreldines, 2026.
//! Tracking state for spawned child sessions.
//! [`SubagentInfo`] is the single source of truth, used by both the subagent pane (display) and the permission view (provenance labels).
//!
//! # Child-transcript replay and eviction
//!
//! - **replay**: read a child's persisted `updates.jsonl` and apply it to that child's view.
//!   [`ensure_subagent_child_replayed`] runs on fullscreen open and dashboard attach.
//!   [`replay_resumed_child_before_live_block`] runs only through its single funnel,
//!   the [`child_view_for_live_update_mut`](crate::app::agent_view::AgentView::child_view_for_live_update_mut) accessor.
//!   The funnel reads a resumed child's inherited history before its first live block closes the empty-view window.
//! - **evict**: drop a finished child's retained view once disk is proven able to rebuild it ([`evict_finished_child_view`]).
//!
//! The ordering rule both depend on: a replay may only append to a view that *shows nothing yet*.
//! Disk history can therefore never land after a live block.
//! A finished foreground child is reset to that state first; a child that is still running, or a background child, waits instead.
//!
//! The child's session stream is the only writer of its task prompt (the shell's `UserMessageChunk` echo, live and persisted); the pager seeds no copy.
//! A `UserPrompt` in a running child's view therefore means the live stream already reached it, and a fresh child's disk holds nothing newer.
//!
//! The spawn path itself never reads the child transcript (the MB-scale `updates.jsonl`), so a burst of spawns cannot block the UI thread.
//! The small `meta.json` enrichment ([`enrich_from_meta`]) is a separate, bounded read.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use distill_shell::session::storage::{
    ReplayEmission, ReplayLookupFallback, ReplayPathHint, ReplayedUpdate, replay_would_emit,
    stream_replay_updates_at_hinted,
};
use serde::Deserialize;

mod lifecycle;

#[cfg(test)]
pub(crate) use lifecycle::SUBAGENT_ATTEMPT_HISTORY_LIMIT;
pub(crate) use lifecycle::{
    AcceptedSubagentLifecycle, SubagentAttemptKey, SubagentLifecycleEffect,
    SubagentLifecycleReduction, SubagentLifecycleState, SubagentLifecycleTransition,
};

#[derive(Debug, Clone)]
pub struct SubagentAttemptInfo {
    pub(crate) lifecycle: SubagentLifecycleState,
    pub persona: Option<Arc<str>>,
    pub role: Option<Arc<str>>,
    pub model: Option<Arc<str>>,
    /// "new" or "resumed".
    pub context_source: Option<Arc<str>>,
    pub resumed_from: Option<Arc<str>>,
    /// "read-only", "read-write", "execute", or "all".
    pub capability_mode: Option<Arc<str>>,
    pub workflow_run_id: Option<Arc<str>>,
    /// Whether the context was normalized into `<background_context>`.
    pub context_normalized: bool,
    pub parent_prompt_id: Option<Arc<str>>,
    pub started_at: Instant,
    /// Latest progress/finish update, else `started_at`; the dashboard's "last activity" sort key.
    pub last_progress_at: Instant,
    /// Terminal status from `SubagentFinished`: "completed", "failed", or "cancelled".
    pub status: Option<Arc<str>>,
    pub error: Option<Arc<str>>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: Option<u64>,
    pub tool_calls: Option<u32>,
    pub turns: Option<u32>,

    /// Live progress from `SubagentProgress`.
    pub turn_count: Option<u32>,
    pub tool_call_count: Option<u32>,
    pub tokens_used: Option<u64>,
    pub context_window_tokens: Option<u64>,
    /// 0-100.
    pub context_usage_pct: Option<u8>,
    pub tools_used: Vec<Arc<str>>,
    pub error_count: Option<u32>,
    /// Live activity label ("Thinking", "Running: cargo build") for the tasks pane and dashboard; cleared on `SubagentFinished`.
    pub activity_label: Option<String>,

    /// Affects scrollback rendering (background shows "started:"/"completed:").
    pub is_background: bool,

    /// Set on kill request, cleared on `SubagentFinished`.
    pub pending_kill: bool,
    /// Auto-clears `pending_kill` after a timeout so the user can retry if the kill notification is lost.
    pub kill_requested_at: Option<Instant>,

    /// Started row for the current attempt.
    pub scrollback_entry_id: Option<crate::scrollback::entry::EntryId>,
    /// Terminal row for the current background attempt.
    pub terminal_entry_id: Option<crate::scrollback::entry::EntryId>,
}

impl SubagentAttemptInfo {
    fn preserve_progress_from(&mut self, prior: &Self) {
        self.last_progress_at = prior.last_progress_at;
        self.duration_ms = prior.duration_ms;
        self.turn_count = prior.turn_count;
        self.tool_call_count = prior.tool_call_count;
        self.tokens_used = prior.tokens_used;
        self.context_window_tokens = prior.context_window_tokens;
        self.context_usage_pct = prior.context_usage_pct;
        self.tools_used.clone_from(&prior.tools_used);
        self.error_count = prior.error_count;
        self.activity_label.clone_from(&prior.activity_label);
    }

    pub(crate) fn terminal_update(
        &self,
        subagent_id: &str,
        child_session_id: &str,
        fallback_status: &str,
    ) -> distill_shell::extensions::notification::SessionUpdate {
        distill_shell::extensions::notification::SessionUpdate::SubagentFinished {
            subagent_id: subagent_id.to_owned(),
            attempt_id: self.lifecycle.current_attempt_id().map(str::to_owned),
            child_session_id: child_session_id.to_owned(),
            status: self.status.as_deref().unwrap_or(fallback_status).to_owned(),
            error: self.error.as_deref().map(str::to_owned),
            tool_calls: self.tool_calls.unwrap_or(0),
            turns: self.turns.unwrap_or(0),
            duration_ms: self.duration_ms.unwrap_or(0),
            tokens_used: self.tokens_used.unwrap_or(0),
            output: None,
            will_wake: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubagentInfo {
    pub subagent_id: Arc<str>,
    pub child_session_id: Arc<str>,
    pub description: Arc<str>,
    pub subagent_type: Arc<str>,
    pub attempt: SubagentAttemptInfo,
    pub(crate) completed_attempt_tokens: u64,
    pub(crate) sealed_attempt_tokens: HashMap<SubagentAttemptKey, u64>,

    /// Enriched from the on-disk `meta.json`.
    pub prompt: Option<Arc<str>>,
    pub child_cwd: Option<Arc<str>>,
    pub worktree_path: Option<Arc<str>>,

    pub(crate) transcript: ChildTranscript,
}

/// Where a child's authoritative transcript lives.
/// One state feeds both the replay-on-open and the eviction decision, so the two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ChildTranscript {
    /// No disk copy proven yet: the next fullscreen open replays `updates.jsonl`.
    /// A failed read stays here, so a lagging persistence flush is retried.
    /// So does an empty read of a finished child, or of a still-running resumed child whose inherited history is expected on disk.
    #[default]
    NeedsReplay,
    /// An emitting replay proved disk reproduces the transcript: the retained view may be dropped and rebuilt.
    DiskBacked,
    /// A replay of a still-running child that inherits nothing found an empty disk.
    /// The result is cached so later opens skip the relocation scan.
    /// A resumed child never caches here: its inherited history is expected on disk, so an empty read stays `NeedsReplay` to retry.
    DiskEmptyWhileRunning,
    /// The in-memory view is the only copy (disk resolved to nothing while the view held content), so evicting it would lose the transcript.
    MemoryOnly,
}

/// Disk is only final once the child is terminal.
/// An empty read means "not written yet" for a running child and "nothing was ever written" for a finished one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildLifecycle {
    Running,
    Finished,
}

/// A resumed child inherits its source's persisted history, copied into its session dir at spawn.
/// An empty read while it runs is therefore transient ("not visible yet").
/// A fresh or forked child starts with an empty replay transcript, so an empty read is a settled negative worth caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildOrigin {
    Resumed,
    Fresh,
}

impl ChildTranscript {
    pub(crate) fn needs_replay(self) -> bool {
        matches!(self, Self::NeedsReplay)
    }

    pub(crate) fn evictable(self) -> bool {
        matches!(self, Self::DiskBacked)
    }

    /// Only an emitting replay proves the disk copy.
    /// An empty read caches the negative result only for a still-running child that inherits nothing.
    /// A resumed child's inherited history is expected on disk, so its empty-while-running read is transient and must stay `NeedsReplay`.
    fn record_replay(
        &mut self,
        outcome: &std::io::Result<ReplayEmission>,
        lifecycle: ChildLifecycle,
        origin: ChildOrigin,
    ) {
        debug_assert!(self.needs_replay());
        match (outcome, lifecycle, origin) {
            (Ok(ReplayEmission::Emitted), _, _) => *self = Self::DiskBacked,
            (Ok(ReplayEmission::Empty), ChildLifecycle::Running, ChildOrigin::Fresh) => {
                *self = Self::DiskEmptyWhileRunning
            }
            (Ok(ReplayEmission::Empty), ChildLifecycle::Running, ChildOrigin::Resumed)
            | (Ok(ReplayEmission::Empty), ChildLifecycle::Finished, _)
            | (Err(_), _, _) => {}
        }
    }

    /// The child is terminal, so disk is final and the cached empty read is worth one more try.
    /// A proven `DiskBacked` or `MemoryOnly` state is untouched.
    pub(crate) fn retry_disk_after_finish(&mut self) {
        if matches!(self, Self::DiskEmptyWhileRunning) {
            *self = Self::NeedsReplay;
        }
    }

    /// A new attempt can append beyond an earlier disk proof, so persistence must be proven again.
    pub(crate) fn begin_new_attempt(&mut self) {
        if matches!(self, Self::DiskBacked | Self::DiskEmptyWhileRunning) {
            *self = Self::NeedsReplay;
        }
    }

    /// A full parent replay means disk may now contain a tail that arrived while disconnected.
    pub(crate) fn begin_parent_replay(&mut self) {
        *self = Self::NeedsReplay;
    }

    /// The view was reset to the empty baseline: rebuild on next open.
    pub(crate) fn evicted(&mut self) {
        debug_assert!(
            !matches!(self, Self::MemoryOnly),
            "evicting a MemoryOnly transcript would lose its only copy"
        );
        *self = Self::NeedsReplay;
    }

    pub(crate) fn discovered_memory_only(&mut self) {
        debug_assert!(
            !self.evictable(),
            "must not downgrade a proven DiskBacked copy to MemoryOnly"
        );
        *self = Self::MemoryOnly;
    }
}

pub(crate) struct SubagentChildInfo {
    pub(crate) subagent_id: String,
    pub(crate) child_session_id: String,
    pub(crate) description: String,
    pub(crate) subagent_type: String,
}

impl SubagentInfo {
    pub(crate) fn from_spawn(
        prior: Option<Self>,
        child: SubagentChildInfo,
        mut attempt: SubagentAttemptInfo,
        is_new_attempt: bool,
    ) -> Self {
        match prior {
            Some(mut info) => {
                if is_new_attempt {
                    info.transcript.begin_new_attempt();
                } else {
                    attempt.preserve_progress_from(&info.attempt);
                }
                info.subagent_id = Arc::from(child.subagent_id);
                info.child_session_id = Arc::from(child.child_session_id);
                info.description = Arc::from(child.description);
                info.subagent_type = Arc::from(child.subagent_type);
                info.attempt = attempt;
                info
            }
            None => Self {
                subagent_id: Arc::from(child.subagent_id),
                child_session_id: Arc::from(child.child_session_id),
                description: Arc::from(child.description),
                subagent_type: Arc::from(child.subagent_type),
                attempt,
                completed_attempt_tokens: 0,
                sealed_attempt_tokens: HashMap::new(),
                prompt: None,
                child_cwd: None,
                worktree_path: None,
                transcript: Default::default(),
            },
        }
    }

    pub(crate) fn seal_current_attempt_tokens(&mut self) {
        let Some(attempt_key) = self.attempt.lifecycle.current_attempt_key().cloned() else {
            return;
        };
        let tokens = self.attempt.tokens_used.unwrap_or(0);
        self.seal_attempt_tokens(attempt_key, tokens);
    }

    pub(crate) fn seal_attempt_tokens(&mut self, attempt_key: SubagentAttemptKey, tokens: u64) {
        let prior = self
            .sealed_attempt_tokens
            .insert(attempt_key, tokens)
            .unwrap_or(0);
        self.completed_attempt_tokens = self
            .completed_attempt_tokens
            .saturating_sub(prior)
            .saturating_add(tokens);
    }

    pub fn is_running(&self) -> bool {
        !self.attempt.lifecycle.is_finished()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.attempt.lifecycle.is_finished()
    }

    #[cfg(test)]
    pub(crate) fn set_finished_for_test(&mut self, finished: bool) {
        self.attempt.lifecycle.set_finished_for_test(finished);
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.attempt.started_at.elapsed()
    }

    /// Uses the authoritative `duration_ms` from `SubagentFinished` when available, else the live wall-clock elapsed.
    pub fn display_elapsed(&self) -> std::time::Duration {
        if self.is_finished() {
            self.attempt
                .duration_ms
                .map(std::time::Duration::from_millis)
                .unwrap_or_else(|| self.elapsed())
        } else {
            self.elapsed()
        }
    }
}

/// Pager-side slice of the shell's on-disk `SubagentMeta`.
#[derive(Debug, Deserialize)]
struct SubagentMetaSlice {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    child_cwd: Option<String>,
    #[serde(default)]
    worktree_path: Option<String>,
}

/// Grok home for the replay path (overridable in tests).
#[cfg(not(test))]
fn effective_distill_home() -> std::path::PathBuf {
    distill_shell::util::distill_home::distill_home()
}

#[cfg(test)]
thread_local! {
    static REPLAY_GROK_HOME: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Override grok home for disk-replay unit tests (thread-local).
#[cfg(test)]
pub(crate) fn set_replay_distill_home_for_tests(home: Option<std::path::PathBuf>) {
    REPLAY_GROK_HOME.with(|h| *h.borrow_mut() = home);
}

#[cfg(test)]
fn effective_distill_home() -> std::path::PathBuf {
    if let Some(home) = REPLAY_GROK_HOME.with(|h| h.borrow().clone()) {
        return home;
    }
    distill_shell::util::distill_home::distill_home()
}

/// Best-effort enrichment from the shell's on-disk `meta.json`.
pub(crate) fn enrich_from_meta(
    info: &mut SubagentInfo,
    parent_cwd: &std::path::Path,
    parent_session_id: &str,
) {
    enrich_from_meta_with_home(
        info,
        &effective_distill_home(),
        parent_cwd,
        parent_session_id,
    );
}

fn enrich_from_meta_with_home(
    info: &mut SubagentInfo,
    distill_home: &std::path::Path,
    parent_cwd: &std::path::Path,
    parent_session_id: &str,
) {
    let meta_path = distill_home
        .join("sessions")
        .join(urlencoding::encode(&parent_cwd.to_string_lossy()).as_ref())
        .join(parent_session_id)
        .join("subagents")
        .join(info.subagent_id.as_ref())
        .join("meta.json");

    let content = match std::fs::read_to_string(&meta_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "meta.json not found");
            return;
        }
    };

    let meta: SubagentMetaSlice = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "meta.json parse failed");
            return;
        }
    };

    info.prompt = meta.prompt.map(Arc::from);
    info.child_cwd = meta.child_cwd.map(Arc::from);
    info.worktree_path = meta.worktree_path.map(Arc::from);
}

/// Best-effort streamed replay of a child's inherited conversation.
/// `Err`: the read failed, so callers must not mark the child replayed.
/// The `child_cwd` hint skips the full relocation scan when it matches.
fn replay_inherited_updates(
    child_view: &mut crate::app::agent_view::AgentView,
    child_session_id: &str,
    parent_cwd: &std::path::Path,
    child_cwd: Option<&std::path::Path>,
    fallback: ReplayLookupFallback,
) -> std::io::Result<ReplayEmission> {
    let home = effective_distill_home();
    let hint = ReplayPathHint {
        parent_cwd: Some(parent_cwd),
        child_cwd,
        fallback,
    };
    #[cfg(test)]
    test_support::record_transcript_read();

    child_view.scrollback.begin_batch();
    let outcome = stream_replay_updates_at_hinted(child_session_id, &home, hint, |update| {
        match update {
            ReplayedUpdate::Acp(update, meta) => {
                // Rebuilt entries keep their on-disk timestamps, not the rebuild time.
                let mut meta = crate::acp::meta::NotificationMeta::from_json(meta.as_ref());
                meta.is_replay = true;
                child_view
                    .session
                    .handle_update(update, &meta, &mut child_view.scrollback);
            }
            ReplayedUpdate::Xai(update) => {
                // Same window as `session/load`: historical Grok events must not start live commands
                // (family-switch compact) or defer compact outcomes to a turn that never comes.
                let was_loading = child_view.session.loading_replay;
                child_view.session.loading_replay = true;
                crate::app::acp_handler::apply_child_view_session_event(child_view, &update, false);
                child_view.session.loading_replay = was_loading;
            }
        }
    });
    child_view.scrollback.end_batch();
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!(session_id = %child_session_id, error = %e, "failed to read updates for replay");
            return Err(e);
        }
    };

    // Purge only after real work: nothing emitted means nothing to reclaim.
    if outcome == ReplayEmission::Emitted {
        crate::memory_release::release_retained_memory("subagent-replay");
    }
    Ok(outcome)
}

/// Counts `updates.jsonl` open attempts (per thread) so a test can assert a path did no disk work.
#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        static TRANSCRIPT_READS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record_transcript_read() {
        TRANSCRIPT_READS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn transcript_reads() -> usize {
        TRANSCRIPT_READS.with(Cell::get)
    }

    /// Baseline [`super::SubagentInfo`] fixture: a running, foreground, non-resumed "explore" child.
    /// It is shared by the `#[path]`-included test modules (`subagent_tests`, `subagent_format_tests`).
    pub(crate) fn make_info() -> super::SubagentInfo {
        let now = std::time::Instant::now();
        super::SubagentInfo {
            subagent_id: "sa-1".into(),
            child_session_id: "cs-1".into(),
            description: "test task".into(),
            subagent_type: "explore".into(),
            attempt: super::SubagentAttemptInfo {
                lifecycle: crate::app::subagent::SubagentLifecycleState::running_legacy_for_test(),
                persona: None,
                role: None,
                model: None,
                context_source: None,
                resumed_from: None,
                capability_mode: None,
                workflow_run_id: None,
                context_normalized: false,
                parent_prompt_id: None,
                started_at: now,
                last_progress_at: now,
                status: None,
                error: None,
                duration_ms: None,
                tool_calls: None,
                turns: None,
                turn_count: None,
                tool_call_count: None,
                tokens_used: None,
                context_window_tokens: None,
                context_usage_pct: None,
                tools_used: Vec::new(),
                error_count: None,
                activity_label: None,
                is_background: false,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                terminal_entry_id: None,
            },
            completed_attempt_tokens: 0,
            sealed_attempt_tokens: Default::default(),
            prompt: None,
            child_cwd: None,
            worktree_path: None,
            transcript: Default::default(),
        }
    }
}

/// True when a scrollback holds nothing but `TurnCompleted` footers.
/// A finalize recreates that content, so it must not pin the view `MemoryOnly`.
/// A `UserPrompt` counts as content: it came from the session stream and may be the only copy.
fn scrollback_is_footer_only(scrollback: &crate::scrollback::state::ScrollbackState) -> bool {
    for i in 0..scrollback.len() {
        let Some(entry) = scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            crate::scrollback::block::RenderBlock::SessionEvent(b)
                if matches!(
                    b.event,
                    crate::scrollback::blocks::SessionEvent::TurnCompleted { .. }
                ) => {}
            _ => return false,
        }
    }
    true
}

/// What [`ensure_subagent_child_replayed`] did with a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum ChildReplayOutcome {
    /// The replay emitted content; the disk copy is now recorded `DiskBacked`.
    Replayed,
    /// The read succeeded but found nothing on disk yet; the transcript stays unsettled so a later open retries.
    FoundNothingOnDisk,
    /// The read failed; the transcript stays `NeedsReplay` to retry.
    ReadFailed,
    /// The transcript is already accounted for, so nothing was read.
    NothingToRead,
    /// A running view already holds live blocks; disk is not read.
    ViewHoldsLiveBlocks,
    /// No `SubagentInfo` or no view under this id (pruned tab, stale id).
    UnknownChild,
}

/// Replay child `updates.jsonl` on fullscreen open (and dashboard attach) when not yet read.
/// A finished child always rebuilds from disk.
/// A running view is filled only while it still shows nothing.
pub(crate) fn ensure_subagent_child_replayed(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> ChildReplayOutcome {
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return ChildReplayOutcome::UnknownChild;
    };
    if !info.transcript.needs_replay() {
        return ChildReplayOutcome::NothingToRead;
    }
    let finished = info.is_finished();
    let is_background = info.attempt.is_background;
    let resumed = is_resumed_child(info);
    // A child that finished during a resume defers the live finalize; reapply the footer after load
    let finished_elapsed = finished
        .then_some(info.attempt.duration_ms)
        .flatten()
        .map(std::time::Duration::from_millis);
    let Some(child_view) = parent.subagent_views.get(child_sid) else {
        return ChildReplayOutcome::UnknownChild;
    };
    // Ordering barrier: a running view is filled only while it holds nothing.
    // A finished replayed child rebuilds because disk is now authoritative for the disconnected tail.
    if !finished && !child_view.scrollback.is_empty() {
        tracing::debug!(
            child_session_id = %child_sid,
            finished,
            is_background,
            "skipping child transcript replay: the view already holds live blocks"
        );
        return ChildReplayOutcome::ViewHoldsLiveBlocks;
    }
    // Reset to the evicted baseline first: the rebuild trusts disk only, never appending onto a stray or unpersisted live block
    let detached_state = if finished {
        let detached_state = detach_child_view_content(parent, child_sid);
        debug_assert!(
            parent
                .subagent_views
                .get(child_sid)
                .is_none_or(|view| view.scrollback.is_empty()),
            "the reset must leave the view empty, \
             or the replay below appends disk history after live blocks"
        );
        detached_state
    } else {
        None
    };
    // A finished rebuild or resumed source may be relocated
    // A running child stays hinted-only, since a copy with the same id under a foreign cwd is not its own
    let fallback = if finished || resumed {
        ReplayLookupFallback::Relocation
    } else {
        ReplayLookupFallback::HintedOnly
    };
    let outcome = replay_child_and_record_outcome(parent, child_sid, fallback);
    restore_or_finalize_after_replay(
        parent,
        child_sid,
        &outcome,
        detached_state,
        finished_elapsed,
    );
    match outcome {
        Ok(ReplayEmission::Emitted) => ChildReplayOutcome::Replayed,
        Ok(ReplayEmission::Empty) => ChildReplayOutcome::FoundNothingOnDisk,
        Err(_) => ChildReplayOutcome::ReadFailed,
    }
}

/// The tail of [`ensure_subagent_child_replayed`].
/// Given the replay outcome and the pre-reset detached content, it either restores that content or stamps the finished footer.
/// A read error, or a detached view that was only a footer, is left dropped and `NeedsReplay` so the next open retries.
fn restore_or_finalize_after_replay(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
    outcome: &std::io::Result<ReplayEmission>,
    detached_state: Option<crate::app::agent_view::ReplayRebuiltState>,
    finished_elapsed: Option<std::time::Duration>,
) {
    // A rebuild that emitted nothing keeps the only in-memory copy; a footer-only view is not content, so leave it dropped and NeedsReplay
    let restore = match outcome {
        Ok(ReplayEmission::Emitted) => false,
        Ok(ReplayEmission::Empty) => detached_state
            .as_ref()
            .is_some_and(|t| !scrollback_is_footer_only(&t.scrollback)),
        Err(_) => true,
    };
    let mut restored = false;
    if restore
        && let Some(detached_state) = detached_state
        && let Some(child_view) = parent.subagent_views.get_mut(child_sid)
    {
        child_view.restore_replay_rebuilt_state(detached_state);
        restored = true;
        // Populated restore after an empty read: this is the only copy, exempt from eviction.
        if matches!(outcome, Ok(ReplayEmission::Empty))
            && let Some(info) = parent.subagent_sessions.get_mut(child_sid)
        {
            info.transcript.discovered_memory_only();
        }
    }
    let parent_turn_running =
        parent.session.state.is_turn_running() || parent.session.state.is_cancelling();
    if let Some(child_view) = parent.subagent_views.get_mut(child_sid) {
        match finished_elapsed {
            // No footer on a failed rebuild (retry re-applies it) or restored content (already stamped; appending doubles it)
            Some(elapsed) if outcome.is_ok() && !restored => {
                finalize_finished_child_view(child_view, elapsed)
            }
            Some(_) => {}
            None if !parent_turn_running => {
                // Parent died mid-run with no live turn: sweep stuck running entries so they can't hold needs_animation() open forever
                child_view.scrollback.finish_all_running();
            }
            None => {}
        }
    }
}

fn is_resumed_child(info: &SubagentInfo) -> bool {
    info.attempt.resumed_from.is_some() || info.attempt.context_source.as_deref() == Some("resumed")
}

/// A resumed child's source transcript is copied into its session dir, and the live stream never repeats it.
/// A non-resumed child needs nothing: its `updates.jsonl` only ever holds blocks the live stream already delivered.
/// Idempotent and self-gating: only a resumed child still in `NeedsReplay` with an empty view is filled.
pub(crate) fn replay_resumed_child_before_live_block(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) {
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return;
    };
    if !info.transcript.needs_replay() || !is_resumed_child(info) {
        return;
    }
    if !parent
        .subagent_views
        .get(child_sid)
        .is_some_and(|view| view.scrollback.is_empty())
    {
        return;
    }
    let _ = ensure_subagent_child_replayed(parent, child_sid);
}

/// Replay the child's on-disk transcript and record what the read proved on [`SubagentInfo::transcript`] (see [`ChildTranscript::record_replay`]).
fn replay_child_and_record_outcome(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
    fallback: ReplayLookupFallback,
) -> std::io::Result<ReplayEmission> {
    let parent_cwd = parent.session.cwd.clone();
    let child_cwd = parent
        .subagent_sessions
        .get(child_sid)
        .and_then(|info| info.child_cwd.clone());
    let mut outcome = Ok(ReplayEmission::Empty);
    if let Some(child_view) = parent.subagent_views.get_mut(child_sid) {
        outcome = replay_inherited_updates(
            child_view,
            child_sid,
            &parent_cwd,
            child_cwd.as_deref().map(std::path::Path::new),
            fallback,
        );
    }
    if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
        let lifecycle = if info.is_finished() {
            ChildLifecycle::Finished
        } else {
            ChildLifecycle::Running
        };
        let origin = if is_resumed_child(info) {
            ChildOrigin::Resumed
        } else {
            ChildOrigin::Fresh
        };
        info.transcript.record_replay(&outcome, lifecycle, origin);
    }
    outcome
}

/// Reset a child view to the empty baseline: detach every replay-rebuilt field and drop the media caches.
///
/// Returns the detached state so a rebuild that emitted nothing can restore it losslessly (eviction drops it instead).
#[must_use = "dropping the detached state destroys the only in-memory copy; eviction must drop it explicitly"]
fn detach_child_view_content(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> Option<crate::app::agent_view::ReplayRebuiltState> {
    let child_view = parent.subagent_views.get_mut(child_sid)?;
    let detached = child_view.take_replay_rebuilt_state();
    // Drop the byte cache and failed-load markers; keep inline_media_ids so transmitted placements stay valid and re-place from disk
    child_view.inline_media_cache = Default::default();
    child_view.inline_media_load_failed = Default::default();
    Some(detached)
}

/// Whether [`evict_finished_child_view`] dropped the retained view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum EvictOutcome {
    /// The retained transcript was dropped; the first open rebuilds from disk.
    Evicted,
    /// A guard applied (open fullscreen, unfinished or background, memory-only, or an unproven disk probe); the caller must finalize in place.
    Retained,
}

/// Returns [`EvictOutcome::Retained`] when a guard applies and the caller must finalize in place.
/// The guards: the child open fullscreen, unfinished or background children, and memory-only transcripts.
/// A view holding content is dropped only once a disk probe proves the persisted transcript would emit.
pub(crate) fn evict_finished_child_view(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> EvictOutcome {
    if parent.active_subagent.as_deref() == Some(child_sid) {
        return EvictOutcome::Retained;
    }
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return EvictOutcome::Retained;
    };
    if info.is_running()
        || info.attempt.is_background
        || matches!(info.transcript, ChildTranscript::MemoryOnly)
    {
        return EvictOutcome::Retained;
    }
    let Some(child_view) = parent.subagent_views.get(child_sid) else {
        if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
            info.transcript.evicted();
        }
        return EvictOutcome::Evicted;
    };
    // Purge only after a real drop: re-evicting a bare view frees nothing.
    let had_content =
        !child_view.scrollback.is_empty() || !child_view.inline_media_cache.is_empty();
    if !info.transcript.evictable() && had_content {
        let child_cwd = info.child_cwd.clone();
        // Hinted-only: the probe stays cheap; a relocated copy the hints miss is found by the open-path rebuild
        let hint = ReplayPathHint {
            parent_cwd: Some(&parent.session.cwd),
            child_cwd: child_cwd.as_deref().map(std::path::Path::new),
            fallback: ReplayLookupFallback::HintedOnly,
        };
        // Anything short of proof keeps the only copy as NeedsReplay, so a late flush is still picked up
        if !replay_would_emit(child_sid, &effective_distill_home(), hint).unwrap_or(false) {
            return EvictOutcome::Retained;
        }
    }
    if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
        info.transcript.evicted();
    }
    drop(detach_child_view_content(parent, child_sid));
    if had_content {
        // Deferred so the purge cost lands between frames, not inside this notification.
        crate::memory_release::request_release_after_draw("subagent-evict");
    }
    EvictOutcome::Evicted
}

/// Finalize a finished child view: end the turn and append the `TurnCompleted` footer.
/// Idempotent on the *trailing* footer: a re-finalized child must not get a second completed line.
/// An earlier turn's `TurnCompleted` deeper in the transcript must not suppress a later turn's footer.
pub(crate) fn finalize_finished_child_view(
    child_view: &mut crate::app::agent_view::AgentView,
    elapsed: std::time::Duration,
) {
    child_view
        .session
        .tracker
        .finish_turn(&mut child_view.scrollback);
    // finish_turn only reaches entries the tracker saw live; entries left running by a from-disk replay would otherwise animate forever
    child_view.scrollback.finish_all_running();
    let already_has_trailing_completed_footer = child_view.scrollback.last().is_some_and(|e| {
        matches!(
            &e.block,
            crate::scrollback::block::RenderBlock::SessionEvent(seb)
                if matches!(
                    seb.event,
                    crate::scrollback::blocks::SessionEvent::TurnCompleted { .. }
                )
        )
    });
    if already_has_trailing_completed_footer {
        return;
    }
    child_view
        .scrollback
        .push_block(crate::scrollback::block::RenderBlock::session_event(
            crate::scrollback::blocks::SessionEvent::TurnCompleted {
                elapsed: Some(elapsed),
            },
        ));
}

fn join_meta_parts(parts: &[Option<&str>]) -> String {
    let non_empty: Vec<&str> = parts.iter().copied().flatten().collect();
    if non_empty.is_empty() {
        String::new()
    } else {
        non_empty.join(" \u{00b7} ")
    }
}

/// Collapse `(persona, role)` to one label when both name the same title.
/// Whitespace-only input counts as absent; the compare is ASCII (registry slugs).
fn dedup_persona_role<'a, 'b>(
    persona: Option<&'a str>,
    role: Option<&'b str>,
) -> (Option<&'a str>, Option<&'b str>) {
    let persona = persona.filter(|s| !s.trim().is_empty());
    let role = role.filter(|s| !s.trim().is_empty());
    match (persona, role) {
        (Some(p), Some(r)) if p.trim().eq_ignore_ascii_case(r.trim()) => (Some(p), None),
        _ => (persona, role),
    }
}

pub(crate) fn format_type_label(subagent_type: &str) -> &str {
    match subagent_type {
        "general-purpose" => "general",
        other => other,
    }
}

pub(crate) fn format_context_badge(info: &SubagentInfo) -> &str {
    match info.attempt.context_source.as_deref() {
        Some("resumed") => "resumed",
        Some("forked") => "forked",
        _ => "",
    }
}

/// Returns `(Some(tag), rest_after_close_bracket)` when the description begins with `[<non-empty>]`, else `(None, description)` unchanged.
pub(crate) fn parse_tag_prefix(description: &str) -> (Option<&str>, &str) {
    if let Some(rest) = description.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        let Some(tag) = rest.get(..close) else {
            return (None, description);
        };
        let tag = tag.trim();
        if !tag.is_empty() {
            let after = rest.get(close + 1..).map_or("", |s| s.trim_start());
            return (Some(tag), after);
        }
    }
    (None, description)
}

/// `format_subagent_label`'s label, then the description in curly quotes when there is one, as the `Subagent`
/// scrollback row quotes it. Clamped by `clamp_activity_subject` (first line, 40 chars) rather than by width:
/// the label is fixed at spawn for rows built later, which have no width to truncate against.
pub(crate) fn subagent_display_label(info: &SubagentInfo) -> String {
    let (label, description) = format_subagent_label(info);
    if description.trim().is_empty() {
        label
    } else {
        format!(
            "{label} \u{201c}{}\u{201d}",
            crate::acp::tracker::clamp_activity_subject(&description)
        )
    }
}

/// Single consolidated label and display description for a subagent row.
/// The description always has the `[tag]` prefix stripped, used as the label or not, so callers never render bracket noise inline.
pub(crate) fn format_subagent_label(info: &SubagentInfo) -> (String, String) {
    let (tag, clean_desc) = parse_tag_prefix(&info.description);

    let raw_label = if let Some(p) = info
        .attempt
        .persona
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        p.to_string()
    } else if let Some(r) = info
        .attempt
        .role
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        r.to_string()
    } else if info.subagent_type.as_ref() != "general-purpose" {
        format_type_label(&info.subagent_type).to_string()
    } else if let Some(tag) = tag {
        tag.to_string()
    } else {
        "general".to_string()
    };

    // Iterating handles uppercase mappings that span several codepoints (`ß` becomes `SS`)
    let mut chars = raw_label.chars();
    let label = match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => raw_label,
    };

    (label, clean_desc.to_string())
}

pub(crate) fn format_subagent_meta(
    persona: Option<&str>,
    role: Option<&str>,
    model: Option<&str>,
) -> String {
    let (persona, role) = dedup_persona_role(persona, role);
    let bare = join_meta_parts(&[persona, role, model]);
    if bare.is_empty() {
        bare
    } else {
        format!(" ({bare})")
    }
}

/// Concise display label for the subagent scrollback block and the fullscreen title bar.
/// Callers handle the `None` activity separately.
pub(crate) fn format_activity_label(activity: &crate::acp::tracker::TurnActivity) -> String {
    use crate::acp::tracker::TurnActivity;
    match activity {
        TurnActivity::Thinking => "Thinking".to_string(),
        TurnActivity::Responding => "Responding".to_string(),
        TurnActivity::ToolRunning { title, description } => {
            if let Some(desc) = description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                crate::acp::tracker::format_waiting_for_subject(desc)
            } else if title.is_empty() {
                "Running tool".to_string()
            } else {
                let first_line = title.lines().next().unwrap_or(title);
                let max_len = crate::acp::tracker::MAX_ACTIVITY_SUBJECT_CHARS;
                // Byte length is the char count for ASCII, so this skips the char walk for the common title
                if first_line.len() <= max_len {
                    format!("Running: {first_line}")
                } else {
                    let char_count = first_line.chars().count();
                    if char_count <= max_len {
                        format!("Running: {first_line}")
                    } else {
                        let truncated: String = first_line.chars().take(max_len).collect();
                        format!("Running: {truncated}\u{2026}")
                    }
                }
            }
        }
        TurnActivity::AutoCompacting => "Compacting".to_string(),
        TurnActivity::Retrying {
            attempt,
            max_retries,
            ..
        } => crate::app::error_display::retry_clause(
            *attempt,
            *max_retries,
            crate::app::error_display::RetryLabelStyle::Compact,
        ),
        TurnActivity::WritingToolCall(writing) => writing.label(),
        TurnActivity::Waiting(reason) => reason.label(),
    }
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "subagent_format_tests.rs"]
mod format_tests;
