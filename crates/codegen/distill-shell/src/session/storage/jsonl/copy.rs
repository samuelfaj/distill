// Modified for Distill by Samuel Fajreldines, 2026.
//! Session fork/copy for the JSONL adapter.
//!
//! The `updates.jsonl` transcript is unbounded, so the copy streams it line by line.
//! Peak memory tracks a single capped line, plus one small per-line record when a prompt cut is requested.
//! Chat history stays materialized: its transforms need random access and the compacted history is bounded by the context window.

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::ops::ControlFlow;
use std::path::Path;

use agent_client_protocol as acp;

use crate::sampling::{
    ConversationItem, conversation_truncate_for_prompt, fork_filter_chat,
    transform_conversation_cwd,
};
use crate::session::info::Info;
use crate::session::persistence::{CHAT_FORMAT_VERSION, Summary};
use crate::session::storage::jsonl::{JsonlStorageAdapter, transform_session_id_in_update};
use crate::session::storage::{
    CopySessionOptions, CopySessionResult, RewindStep, SessionUpdate, SessionUpdateEnvelope,
    filter_rewind_by, rewind_step_for_line, truncate_for_prompt_by,
};

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;

fn is_orchestration_projection_update(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::Xai(notification)
            if matches!(
                &notification.update,
                crate::extensions::notification::SessionUpdate::WorkflowUpdated { .. }
                    | crate::extensions::notification::SessionUpdate::GoalUpdated { .. }
            )
    )
}

/// Updates written plus the `compaction_checkpoints/{uuid}.json` files the surviving records reference, collected in the same pass.
#[derive(Default)]
struct CopiedUpdates {
    count: usize,
    checkpoint_files: BTreeSet<String>,
}

/// Anything past this cap is corruption (e.g. a tail that lost its newlines) and is discarded without being buffered.
/// Discarded lines consume no index in either pass, unlike torn lines, which classify as [`RewindStep::Other`] and end a user run.
const MAX_UPDATE_LINE_BYTES: usize = 64 * 1024 * 1024;

/// [`for_each_jsonl_line_capped`] with the production cap.
fn for_each_jsonl_line<R: BufRead>(
    reader: R,
    f: impl FnMut(usize, &[u8]) -> io::Result<ControlFlow<()>>,
) -> io::Result<()> {
    for_each_jsonl_line_capped(reader, MAX_UPDATE_LINE_BYTES, f)
}

/// Lines over `cap` content bytes are discarded without being buffered whole and consume no index.
/// `f` gets raw bytes rather than the typed `UpdatesIterator`.
/// Classification must tolerate non-UTF-8 lines, and both copy passes need identical line indexes.
fn for_each_jsonl_line_capped<R: BufRead>(
    mut reader: R,
    cap: usize,
    mut f: impl FnMut(usize, &[u8]) -> io::Result<ControlFlow<()>>,
) -> io::Result<()> {
    let mut buf = Vec::new();
    let mut index = 0;
    let mut discarded = 0usize;
    let result = loop {
        buf.clear();
        let n = reader
            .by_ref()
            .take(cap as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            break Ok(());
        }
        if buf.len() > cap && buf.last() != Some(&b'\n') {
            discarded += 1;
            if discarded == 1 {
                tracing::warn!(
                    max_bytes = cap,
                    "discarding over-long updates.jsonl line during fork copy"
                );
            }
            // Drain the remainder of the line without retaining it.
            loop {
                buf.clear();
                let n = reader
                    .by_ref()
                    .take(cap as u64)
                    .read_until(b'\n', &mut buf)?;
                if n == 0 || buf.last() == Some(&b'\n') {
                    break;
                }
            }
            continue;
        }
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        if f(index, line)?.is_break() {
            break Ok(());
        }
        index += 1;
    };
    if discarded > 1 {
        tracing::warn!(
            discarded,
            max_bytes = cap,
            "discarded over-long updates.jsonl lines during fork copy"
        );
    }
    result
}

/// Indexes (in non-empty-line order) of the source lines that survive rewind filtering and the `target_prompt_index` cut.
/// The scan holds one classification per line instead of the lines.
/// As in replay, an unparseable line classifies as [`RewindStep::Other`] (ending a user run) and is skipped later at parse.
fn surviving_line_indexes<R: BufRead>(
    reader: R,
    target_prompt_index: usize,
) -> io::Result<Vec<usize>> {
    struct LineRecord {
        index: usize,
        step: RewindStep,
    }
    let mut records = Vec::new();
    for_each_jsonl_line(reader, |index, line| {
        let step = std::str::from_utf8(line).map_or(RewindStep::Other, rewind_step_for_line);
        records.push(LineRecord { index, step });
        Ok(ControlFlow::Continue(()))
    })?;
    let mut records = filter_rewind_by(records, |record| record.step);
    let keep = truncate_for_prompt_by(&records, target_prompt_index, |record| record.step);
    records.truncate(keep);
    Ok(records.into_iter().map(|record| record.index).collect())
}

/// Streaming writer for the fork target's `updates.jsonl`.
/// Corruption-tolerant like the load path: a torn or undecodable line is skipped with a warning instead of failing the fork.
struct UpdateLineWriter<'a> {
    writer: BufWriter<std::fs::File>,
    source: &'a Path,
    target_session_id: &'a acp::SessionId,
    copied: CopiedUpdates,
    skipped_lines: usize,
}

impl<'a> UpdateLineWriter<'a> {
    fn try_new(
        target: &Path,
        source: &'a Path,
        target_session_id: &'a acp::SessionId,
    ) -> io::Result<Self> {
        Ok(Self {
            writer: BufWriter::new(std::fs::File::create(target)?),
            source,
            target_session_id,
            copied: CopiedUpdates::default(),
            skipped_lines: 0,
        })
    }

    fn copy_line(&mut self, line: &[u8]) -> io::Result<()> {
        let update = match std::str::from_utf8(line).map(SessionUpdateEnvelope::from_str) {
            Ok(Ok(update)) => update,
            Ok(Err(error)) => {
                self.skip_torn_line(&error);
                return Ok(());
            }
            Err(error) => {
                self.skip_torn_line(&error);
                return Ok(());
            }
        };
        if is_orchestration_projection_update(&update) {
            return Ok(());
        }
        if let SessionUpdate::Xai(notification) = &update
            && let crate::extensions::notification::SessionUpdate::CompactionCheckpoint(info) =
                &notification.update
        {
            self.copied
                .checkpoint_files
                .insert(info.checkpoint_file.clone());
        }
        let update = transform_session_id_in_update(update, self.target_session_id);
        let envelope = SessionUpdateEnvelope::from_update(&update).map_err(invalid_data)?;
        serde_json::to_writer(&mut self.writer, &envelope).map_err(invalid_data)?;
        self.writer.write_all(b"\n")?;
        self.copied.count += 1;
        Ok(())
    }

    fn skip_torn_line(&mut self, error: &dyn std::fmt::Display) {
        self.skipped_lines += 1;
        if self.skipped_lines == 1 {
            tracing::warn!(
                error = %error,
                path = %self.source.display(),
                "skipping unparseable updates.jsonl line during fork copy (torn append?)"
            );
        }
    }

    fn finish(mut self) -> io::Result<CopiedUpdates> {
        // The first skipped line already warned with its parse error.
        if self.skipped_lines > 1 {
            tracing::warn!(
                skipped = self.skipped_lines,
                copied = self.copied.count,
                path = %self.source.display(),
                "skipped unparseable session update lines during fork copy"
            );
        }
        self.writer.flush()?;
        Ok(self.copied)
    }
}

/// Copy `source` (an `updates.jsonl`) to `target` without materializing it.
/// Without one, every line streams through, preserving rewind markers and dead branches.
/// `updates.jsonl` is append-only by contract, so lines appended after pass one land past every survivor index.
fn copy_updates_streaming(
    source: &Path,
    target: &Path,
    target_session_id: &acp::SessionId,
    target_prompt_index: Option<usize>,
) -> io::Result<CopiedUpdates> {
    let mut writer = UpdateLineWriter::try_new(target, source, target_session_id)?;
    let mut file = match std::fs::File::open(source) {
        Ok(file) => file,
        // A missing source is an empty transcript; still write the target.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return writer.finish(),
        Err(error) => return Err(error),
    };
    match target_prompt_index {
        None => {
            for_each_jsonl_line(BufReader::new(file), |_, line| {
                writer.copy_line(line)?;
                Ok(ControlFlow::Continue(()))
            })?;
        }
        Some(target_idx) => {
            let survivors = surviving_line_indexes(BufReader::new(&mut file), target_idx)?;
            file.seek(io::SeekFrom::Start(0))?;
            let mut survivors = survivors.into_iter().peekable();
            for_each_jsonl_line(BufReader::new(file), |index, line| {
                if survivors.next_if_eq(&index).is_some() {
                    writer.copy_line(line)?;
                }
                Ok(if survivors.peek().is_none() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            })?;
        }
    }
    writer.finish()
}

impl JsonlStorageAdapter {
    /// Fully synchronous implementation of `copy_session_data`, for use on a blocking thread; every caller reaches it through `spawn_blocking`.
    pub(crate) fn copy_session_data_sync(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: CopySessionOptions,
    ) -> io::Result<CopySessionResult> {
        // Canonical creator: the fork target chain is born owner-only.
        let target_dir = self.create_session_dir_owner_only(target_info)?;

        let source_summary = self.read_summary_sync(source_info)?;
        let chat_format_version = source_summary.chat_format_version;

        let mut chat_to_copy: Vec<ConversationItem> =
            self.read_chat_history_sync(self.chat_file(source_info), chat_format_version)?;

        if let Some(target_idx) = options.target_prompt_index {
            // +1: the cut keeps the target prompt inclusive.
            let keep = conversation_truncate_for_prompt(&chat_to_copy, target_idx + 1);
            chat_to_copy.truncate(keep);
        }

        if options.fork_filter {
            fork_filter_chat(&mut chat_to_copy);
        }

        for target in [
            self.workflows_dir(target_info),
            self.goal_mode_state_file(target_info)
                .parent()
                .expect("goal state has a parent")
                .to_path_buf(),
        ] {
            match std::fs::remove_dir_all(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        // The child inherits everything below this boundary; compaction preserves it
        let inherited_prefix_len = if options.fork_filter {
            Some(chat_to_copy.len())
        } else {
            options.inherited_prefix_len
        };

        // Worktree forks skip the cwd rewrite: their display_cwd already shows the model the original project path
        // Rewritten conversation paths would contradict it
        if !options.skip_cwd_transform && source_info.cwd != target_info.cwd {
            transform_conversation_cwd(&mut chat_to_copy, &source_info.cwd, &target_info.cwd);
        }

        if options.strip_reasoning {
            chat_to_copy =
                distill_chat_state::compaction_utils::strip_reasoning_blocks(chat_to_copy);
        }

        let num_chat_messages = chat_to_copy.len();
        let cwd_switch_bookkeeping_generation = chat_to_copy
            .iter()
            .filter_map(ConversationItem::working_directory_switch_generation)
            .max()
            .unwrap_or(0);

        // Release chat history before the (typically much larger) updates copy.
        {
            let mut writer = BufWriter::new(std::fs::File::create(self.chat_file(target_info))?);
            for item in &chat_to_copy {
                serde_json::to_writer(&mut writer, item).map_err(invalid_data)?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
        }
        drop(chat_to_copy);

        // A fork_filter copy (subagent context bootstrap) starts the child with an empty replay transcript, so the source updates are never read
        let copied_updates = if options.fork_filter {
            std::fs::write(self.updates_file(target_info), b"")?;
            CopiedUpdates::default()
        } else {
            copy_updates_streaming(
                &self.updates_file(source_info),
                &self.updates_file(target_info),
                &target_info.id,
                options.target_prompt_index,
            )?
        };
        let checkpoint_files = copied_updates.checkpoint_files;
        let num_messages = copied_updates.count;

        let target_summary = fork_summary(
            source_summary,
            target_info,
            &options,
            ForkCounters {
                num_messages,
                num_chat_messages,
                cwd_switch_bookkeeping_generation,
                inherited_prefix_len,
            },
        );
        let minted_agent_id = target_summary.agent_id.clone();
        let summary_bytes = serde_json::to_vec_pretty(&target_summary).map_err(invalid_data)?;
        std::fs::write(self.summary_file(target_info), summary_bytes)?;

        let plan_copied = copy_sidecar_file(
            options.copy_plan_state,
            &self.plan_file(source_info),
            &self.plan_file(target_info),
        )?;
        let signals_copied = copy_sidecar_file(
            options.copy_signals,
            &self.signals_file(source_info),
            &self.signals_file(target_info),
        )?;
        let usage_copied = copy_sidecar_file(
            options.copy_usage,
            &self.usage_file(source_info),
            &self.usage_file(target_info),
        )?;
        if let Some(max_turn) = options.target_prompt_index.map(|i| (i + 1) as u32) {
            if usage_copied {
                restamp_copied_usage(
                    &self.usage_file(target_info),
                    &target_info.id,
                    Some(max_turn),
                )?;
            }
            if usage_copied && signals_copied {
                clip_copied_signals_turns(&self.signals_file(target_info), max_turn)?;
            }
        } else if usage_copied {
            restamp_copied_usage(&self.usage_file(target_info), &target_info.id, None)?;
        }
        if usage_copied && !signals_copied {
            // Resume copies billed history without signals, so the child would start at turn 0 and fold new work into inherited rows
            seed_signals_turn_from_usage(
                &self.usage_file(target_info),
                &self.signals_file(target_info),
            )?;
        }
        let plan_mode_state_copied = copy_sidecar_file(
            options.copy_plan_mode_state,
            &self.plan_mode_state_file(source_info),
            &self.plan_mode_state_file(target_info),
        )?;
        let tool_state_copied = copy_sidecar_file(
            options.copy_tool_state,
            &self.session_dir(source_info).join("tool_state.json"),
            &self.session_dir(target_info).join("tool_state.json"),
        )?;
        let announcement_state_copied = copy_sidecar_file(
            options.copy_announcement_state,
            &self.announcement_state_file(source_info),
            &self.announcement_state_file(target_info),
        )?;
        // A truncating or filtering copy can drop the failure announcement from the child's context.
        // The copied state still marks it announced, permanently muting it.
        // End the episodes so still-down servers re-announce, the same rule as after rewind or compaction.
        if announcement_state_copied
            && (options.target_prompt_index.is_some() || options.fork_filter)
        {
            clear_announced_failure_episodes(&self.announcement_state_file(target_info))?;
        }

        // Title-refresh watermark: only a managed parent (one with a watermark) passes managed state to the child.
        // So a fork of a pre-feature session stays unmanaged (frozen) rather than being adopted.
        // A full fork inherits the parent's checkpoint (keeping the inherited title frozen).
        if let Some(parent_idx) =
            crate::session::helpers::session_summary::load_title_refresh_watermark(
                &self.session_dir(source_info),
            )
        {
            let child_idx = if options.target_prompt_index.is_none() {
                parent_idx
            } else {
                0
            };
            crate::session::helpers::session_summary::save_title_refresh_watermark(
                &self.session_dir(target_info),
                child_idx,
            );
        }

        // Copied verbatim: the archive is immutable, so no cwd rewrite.
        let compaction_segments_copied = if options.copy_compaction_segments {
            let src_dir = self
                .session_dir(source_info)
                .join(distill_compaction_transcript::COMPACTION_DIR);
            let mut copied = 0usize;
            if src_dir.is_dir() {
                let dst_dir = self
                    .session_dir(target_info)
                    .join(distill_compaction_transcript::COMPACTION_DIR);
                std::fs::create_dir_all(&dst_dir)?;
                for entry in std::fs::read_dir(&src_dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        std::fs::copy(entry.path(), dst_dir.join(entry.file_name()))?;
                        copied += 1;
                    }
                }
            }
            copied
        } else {
            0
        };

        let compaction_checkpoints_copied = copy_referenced_checkpoints(
            &checkpoint_files,
            &self.session_dir(source_info),
            &target_dir,
            &source_info.id,
        )?;

        self.persist_fork_status(source_info, target_info, &target_dir, &options);

        Ok(CopySessionResult {
            chat_messages_copied: num_chat_messages,
            agent_id: minted_agent_id,
            updates_copied: num_messages,
            plan_state_copied: plan_copied,
            plan_mode_state_copied,
            signals_copied,
            tool_state_copied,
            announcement_state_copied,
            compaction_segments_copied,
            compaction_checkpoints_copied,
        })
    }

    fn persist_fork_status(
        &self,
        source_info: &Info,
        target_info: &Info,
        target_dir: &std::path::Path,
        options: &CopySessionOptions,
    ) {
        let kind = options.session_kind.as_deref().unwrap_or("fork");
        if !crate::session::fork_status::should_persist(
            kind,
            options.fork_context_source.as_deref(),
        ) {
            return;
        }
        let status = crate::session::fork_status::capture(
            kind,
            &source_info.cwd,
            &target_info.cwd,
            options.source_workspace_dir.as_deref(),
            options.prompt_display_cwd.as_deref(),
            options.target_prompt_index.is_some(),
        );
        crate::session::fork_status::persist(target_dir, &status);
    }
}

/// Counters produced by this copy that feed the fork target's summary, named so the same-typed counts cannot transpose.
struct ForkCounters {
    num_messages: usize,
    num_chat_messages: usize,
    cwd_switch_bookkeeping_generation: u64,
    inherited_prefix_len: Option<usize>,
}

/// Build the fork target's summary: counters from this copy, fork identity from `options`.
/// Every other field is either inherited from the source or reset as for a fresh session.
fn fork_summary(
    source: Summary,
    target_info: &Info,
    options: &CopySessionOptions,
    counters: ForkCounters,
) -> Summary {
    let target_worktree_identity =
        crate::session::worktree::worktree_identity_for_cwd(&target_info.cwd);
    let identity = options.mint_session_identity.then(|| {
        crate::session::persistence::mint_next_session_identity(source.agent_id.as_deref(), true)
    });
    let mut summary = Summary {
        info: target_info.clone(),
        agent_id: identity.as_ref().map(|identity| identity.agent_id.clone()),
        attempt_id: identity.map(|identity| identity.attempt_id),
        cwd_generation: source.cwd_generation,
        previous_cwd: source.previous_cwd,
        pending_cwd_switch_reminder: None,
        cwd_switch_bookkeeping_generation: counters.cwd_switch_bookkeeping_generation,
        session_summary: source.session_summary,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        num_messages: counters.num_messages,
        num_chat_messages: counters.num_chat_messages,
        current_model_id: options
            .new_model_id
            .clone()
            .map(acp::ModelId::new)
            .unwrap_or(source.current_model_id),
        parent_session_id: options.parent_session_id.clone(),
        forked_at: Some(chrono::Utc::now()),
        collection_id: None,
        next_trace_turn: 0,
        chat_format_version: CHAT_FORMAT_VERSION,
        prompt_display_cwd: options.prompt_display_cwd.clone(),
        session_kind: Some(
            options
                .session_kind
                .clone()
                .unwrap_or_else(|| "fork".to_string()),
        ),
        fork_context_source: options.fork_context_source.clone(),
        fork_parent_prompt_id: options.fork_parent_prompt_id.clone(),
        inherited_prefix_len: counters.inherited_prefix_len,
        hidden: None,
        source_workspace_dir: options.source_workspace_dir.clone(),
        git_root_dir: None,
        git_remotes: Vec::new(),
        head_commit: source.head_commit,
        head_branch: source.head_branch,
        request_id: None,
        // Fresh local distill_home, not inherited from source: the fork lives on this machine.
        distill_home: crate::session::persistence::distill_home_string(),
        last_active_at: source.last_active_at,
        generated_title: source.generated_title,
        // A fork keeps the parent's title, so whether that title was set manually carries over too
        title_is_manual: source.title_is_manual,
        // Re-derived from the target path, never inherited: the source's label describes the parent's worktree, not this one
        worktree_label: target_worktree_identity
            .as_ref()
            .map(|identity| identity.label.clone()),
        agent_name: source.agent_name,
        sandbox_profile: source.sandbox_profile,
        reasoning_effort: source.reasoning_effort,
        // Full forks keep the parent's last turn
        // Partial forks (`target_prompt_index`) may drop that turn, so clear the summary rather than showing work not in the child conversation
        last_turn_summary: if options.target_prompt_index.is_some() {
            None
        } else {
            source.last_turn_summary
        },
        last_turn_summary_prompt_id: if options.target_prompt_index.is_some() {
            None
        } else {
            source.last_turn_summary_prompt_id
        },
        // A recap describes the parent's whole session; a partial fork may not contain that work, so clear it there and keep it for full forks
        last_recap: if options.target_prompt_index.is_some() {
            None
        } else {
            source.last_recap
        },
    };
    if options.session_kind.is_none()
        && let Some(identity) = &target_worktree_identity
    {
        summary.stamp_worktree_identity(identity);
        // An explicitly provided source still wins over the derived one.
        if let Some(source_workspace_dir) = &options.source_workspace_dir {
            summary.source_workspace_dir = Some(source_workspace_dir.clone());
        }
    }
    summary
}

/// Remove `announced_failed_servers` from a copied `announcement_state.json`, preserving every other field (including ones this build doesn't know).
/// A file that doesn't parse is left as copied: the next persist rewrites it.
fn clear_announced_failure_episodes(path: &Path) -> io::Result<()> {
    let bytes = std::fs::read(path)?;
    let Ok(mut state) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(());
    };
    let Some(fields) = state.as_object_mut() else {
        return Ok(());
    };
    if fields.remove("announced_failed_servers").is_none() {
        return Ok(());
    }
    crate::session::storage::write_bytes_atomic(
        path,
        &serde_json::to_vec(&state).map_err(invalid_data)?,
    )
}

fn clip_copied_signals_turns(path: &Path, max_turn: u32) -> io::Result<()> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let Ok(mut signals) = serde_json::from_slice::<crate::session::signals::SessionSignals>(&data)
    else {
        return Ok(());
    };
    if signals.turn_count > max_turn {
        signals.turn_count = max_turn;
    }
    if signals.user_message_count > max_turn {
        signals.user_message_count = max_turn;
    }
    if signals.assistant_message_count > max_turn {
        signals.assistant_message_count = max_turn;
    }
    crate::session::storage::write_bytes_atomic(
        path,
        &serde_json::to_vec(&signals).map_err(invalid_data)?,
    )
}

fn seed_signals_turn_from_usage(usage_path: &Path, signals_path: &Path) -> io::Result<()> {
    let data = match std::fs::read(usage_path) {
        Ok(data) => data,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let Ok(file) = serde_json::from_slice::<crate::session::usage_file::SessionUsageFile>(&data)
    else {
        return Ok(());
    };
    let Some(max_turn) = file.turns.iter().map(|turn| turn.turn_number).max() else {
        return Ok(());
    };
    let signals = crate::session::signals::SessionSignals {
        turn_count: max_turn,
        user_message_count: max_turn,
        ..Default::default()
    };
    crate::session::storage::write_bytes_atomic(
        signals_path,
        &serde_json::to_vec(&signals).map_err(invalid_data)?,
    )
}

fn restamp_copied_usage(
    path: &Path,
    session_id: &acp::SessionId,
    max_turn: Option<u32>,
) -> io::Result<()> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let Ok(mut file) =
        serde_json::from_slice::<crate::session::usage_file::SessionUsageFile>(&data)
    else {
        return Ok(());
    };
    file.session_id = session_id.to_string();
    if let Some(max_turn) = max_turn {
        file.retain_turns_through(max_turn);
    }
    crate::session::storage::write_bytes_atomic(
        path,
        &serde_json::to_vec_pretty(&file).map_err(invalid_data)?,
    )
}

/// Copy one optional sidecar file (plan, signals, tool state, ...) when enabled and present; reports whether a copy happened.
/// A sidecar that exists but is not a regular file is skipped with a warning rather than failing the fork.
fn copy_sidecar_file(enabled: bool, src: &Path, dst: &Path) -> io::Result<bool> {
    if !enabled {
        return Ok(false);
    }
    if !src.is_file() {
        if src.exists() {
            tracing::warn!(
                path = %src.display(),
                "sidecar is not a regular file; skipping copy",
            );
        }
        return Ok(false);
    }
    std::fs::copy(src, dst)?;
    Ok(true)
}

/// Copy the `compaction_checkpoints/{uuid}.json` files referenced by the retained records; returns how many copied.
/// Records are user-editable data, so only the exact path shape this feature writes may resolve and symlinks are never followed.
/// Dangling references are skipped rather than failing the fork (otherwise every /rewind in the target session would fail).
fn copy_referenced_checkpoints(
    checkpoint_files: &BTreeSet<String>,
    source_session_dir: &Path,
    target_dir: &Path,
    source_id: &acp::SessionId,
) -> io::Result<usize> {
    if checkpoint_files.is_empty() {
        return Ok(0);
    }
    // The per-file `symlink_metadata` below only vets the final path component
    // So the intermediate `compaction_checkpoints` dir must itself be a real directory
    // A symlinked dir would resolve every matching name outside the session
    match std::fs::symlink_metadata(source_session_dir.join("compaction_checkpoints")) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(meta) => {
            tracing::warn!(
                file_type = ?meta.file_type(),
                session_id = %source_id,
                "compaction_checkpoints is not a real directory; skipping checkpoint copy",
            );
            return Ok(0);
        }
        // Dir gone means every record is dangling; same policy as a missing checkpoint file
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(
                session_id = %source_id,
                "compaction_checkpoints directory missing; skipping checkpoint copy",
            );
            return Ok(0);
        }
        Err(error) => return Err(error),
    }
    let mut copied = 0usize;
    for checkpoint_file in checkpoint_files {
        let relative = Path::new(checkpoint_file);
        // A doctored record path must not address other session files (e.g. the fork's rewritten updates.jsonl).
        let well_formed = relative.parent() == Some(Path::new("compaction_checkpoints"))
            && relative.extension() == Some("json".as_ref());
        if !well_formed {
            tracing::warn!(
                checkpoint_file = %checkpoint_file,
                session_id = %source_id,
                "skipping compaction checkpoint with unexpected path during copy",
            );
            continue;
        }
        let src = source_session_dir.join(relative);
        match std::fs::symlink_metadata(&src) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(meta) => {
                // This feature only ever writes regular files, so don't follow symlinks planted in the source session
                tracing::warn!(
                    path = %src.display(),
                    file_type = ?meta.file_type(),
                    session_id = %source_id,
                    "compaction checkpoint source is not a regular file; skipping copy",
                );
                continue;
            }
            // Already-dangling record (e.g. a chained fork of a broken session): the copy can't invent the file, so don't fail.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %src.display(),
                    session_id = %source_id,
                    "compaction checkpoint file missing from source; skipping copy",
                );
                continue;
            }
            Err(error) => return Err(error),
        }
        let dst = target_dir.join(relative);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        copied += 1;
    }
    Ok(copied)
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
