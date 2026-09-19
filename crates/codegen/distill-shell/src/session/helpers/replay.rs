// Modified for Distill by Samuel Fajreldines, 2026.
//! Replay pipeline for cross-compaction rewind.
//!
//! When rewinding to a prompt before a compaction boundary, the original messages are gone from the in-memory conversation and `chat_history.jsonl`.
//! This module reconstructs the conversation by streaming `updates.jsonl` and handling `CompactionCheckpoint` / `RewindMarker` entries.

use std::io;
use std::path::Path;

use crate::extensions::notification::{
    CompactionCheckpointFile, CompactionCheckpointInfo, SessionUpdate as XaiSessionUpdate,
};
use crate::sampling::ConversationItem;
use crate::session::storage::{SessionUpdate, UpdatesIterator};

#[derive(Debug)]
pub struct ReplayResult {
    /// The reconstructed conversation, suitable for replacing in-memory state.
    pub conversation: Vec<ConversationItem>,
    /// The prompt index that was reached (should equal the target).
    pub prompt_index_reached: usize,
    /// The original User(user_info) text from before the first compaction.
    /// Extracted from the checkpoint file's `original_user_info` field.
    /// `None` if no checkpoint was encountered, the checkpoint predates the field (schema_version 1 without it), or it could not be read.
    pub original_user_info: Option<String>,
    /// Compaction marker for the rebuilt conversation: `Some(idx)` if a summary survives, else `None`.
    pub last_compaction_prompt_index: Option<usize>,
}

/// Uses raw-line peeking: only lines containing `"compaction_checkpoint"` are parsed, skipping full typed deserialization.
pub fn find_latest_compaction_checkpoint(
    updates_path: &Path,
) -> io::Result<Option<CompactionCheckpointInfo>> {
    use crate::session::storage::RawLinePeek;

    let raw_contents = match std::fs::read_to_string(updates_path) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    if !raw_contents.contains("compaction_checkpoint") {
        return Ok(None);
    }

    let mut latest: Option<CompactionCheckpointInfo> = None;

    for line in raw_contents.lines() {
        if line.trim().is_empty() || !line.contains("compaction_checkpoint") {
            continue;
        }

        let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(line) else {
            continue;
        };

        if env.method != Some("_x.ai/session/update") {
            continue;
        }

        let Some(raw_params) = env.params else {
            continue;
        };

        if let Ok(notification) = serde_json::from_str::<
            crate::extensions::notification::SessionNotification,
        >(raw_params.get())
            && let XaiSessionUpdate::CompactionCheckpoint(info) = notification.update
        {
            latest = Some(*info);
        }
    }

    Ok(latest)
}

/// Replay `updates.jsonl` to reconstruct the conversation at `target_prompt_index`.
/// `RewindMarker`: discards accumulated state beyond the marker's target.
/// `CompactionCheckpoint`: loads the checkpoint, or reads only `original_user_info`, based on whether the target is
/// before or after the compaction boundary.
///
/// # Errors
/// Fails iff the innermost base still installed at EOF is an unreadable checkpoint; any other unreadable
/// checkpoint degrades with a warning.
pub fn replay_to_prompt(
    updates_path: &Path,
    session_dir: &Path,
    target_prompt_index: usize,
) -> io::Result<ReplayResult> {
    let Some(iter) = UpdatesIterator::open(updates_path)? else {
        return Ok(ReplayResult {
            conversation: vec![],
            prompt_index_reached: 0,
            original_user_info: None,
            last_compaction_prompt_index: None,
        });
    };

    let mut state = ReplayState::new(target_prompt_index);

    for update_result in iter {
        let update = match update_result {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(?e, "Skipping malformed update during replay");
                continue;
            }
        };

        state.process_update(&update, session_dir);
    }

    // Flush any trailing partial messages.
    state.flush_pending_user();
    state.flush_pending_agent();

    if let Some(Base { blob: Err(e), .. }) = state.bases.pop_if(|base| base.blob.is_err()) {
        return Err(e);
    }

    // After processing the entire file, the conversation may extend beyond the target
    // `target_prompt_index` means "rewind to before prompt N", so keep prompts 0..N-1 (N prompts total)
    if state.prompt_counter > target_prompt_index {
        if let Some(top) = state.bases.last()
            && target_prompt_index >= top.prompt_index
        {
            let (base_len, base_index) = (top.base_len, top.prompt_index);
            state.truncate_after_base(base_len, target_prompt_index - base_index);
        } else if target_prompt_index == 0 {
            state.conversation.clear();
        } else {
            let truncate_at = state.truncate_target(target_prompt_index);
            let keep =
                crate::sampling::conversation_truncate_for_prompt(&state.conversation, truncate_at);
            state.conversation.truncate(keep);
        }
        state.prompt_counter = target_prompt_index;
    }

    Ok(ReplayResult {
        conversation: state.conversation,
        prompt_index_reached: state.prompt_counter,
        original_user_info: state.original_user_info,
        last_compaction_prompt_index: state.bases.last().map(|base| base.prompt_index),
    })
}

/// One compaction checkpoint installed as the conversation base; `blob: Err` means its file was unreadable.
struct Base {
    prompt_index: usize,
    /// Conversation length right after installation; items below it are the opaque base and are never counted.
    base_len: usize,
    blob: Result<(), io::Error>,
}

struct ReplayState {
    /// The prompt index we're trying to reach.
    target: usize,

    conversation: Vec<ConversationItem>,

    /// Current prompt counter (how many user turns we've seen).
    prompt_counter: usize,

    /// Whether we're inside a contiguous sequence of UserMessageChunk updates (used to count user turns correctly: multiple chunks are one turn).
    in_user_message: bool,

    /// Partial text accumulator for the current user message.
    current_user_text: String,

    current_user_prompt_index: Option<usize>,

    /// The current user run is a persisted mid-turn interjection (`_meta.interjection`).
    current_user_is_interjection: bool,

    /// True once any user chunk with `_meta.promptIndex` has been seen.
    /// Unnumbered user runs after that are mid-turn phantoms (not turns).
    seen_prompt_index_marker: bool,

    /// Partial text accumulator for the current agent message.
    current_agent_text: String,

    has_pending_agent: bool,

    /// Installed checkpoint bases, innermost last: a loaded one clears the stack, an unreadable one stacks on top,
    /// and a rewind marker pops every base above its target. While non-empty, only real `UserMessageChunk` turns count.
    bases: Vec<Base>,

    /// The original User(user_info) text from before the first compaction.
    original_user_info: Option<String>,
}

impl ReplayState {
    fn new(target: usize) -> Self {
        Self {
            target,
            conversation: Vec::new(),
            prompt_counter: 0,
            in_user_message: false,
            current_user_text: String::new(),
            current_user_prompt_index: None,
            current_user_is_interjection: false,
            seen_prompt_index_marker: false,
            current_agent_text: String::new(),
            has_pending_agent: false,
            bases: Vec::new(),
            original_user_info: None,
        }
    }

    fn process_update(&mut self, update: &SessionUpdate, session_dir: &Path) {
        match update {
            SessionUpdate::Xai(notification) => {
                match &notification.update {
                    XaiSessionUpdate::CompactionCheckpoint(info) => {
                        self.handle_checkpoint(info, session_dir);
                    }
                    XaiSessionUpdate::RewindMarker {
                        target_prompt_index,
                        ..
                    } => {
                        self.handle_rewind_marker(*target_prompt_index);
                    }
                    // Other Grok notifications are informational; skip them
                    _ => {}
                }
            }
            SessionUpdate::Acp(notification) => {
                match &notification.update {
                    agent_client_protocol::SessionUpdate::UserMessageChunk(chunk) => {
                        self.handle_user_chunk(chunk);
                    }
                    agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) => {
                        self.handle_agent_chunk(chunk);
                    }
                    _ => {
                        // Other ACP updates (ToolCall, StatusUpdate, etc.) don't affect prompt counting, so replay skips them
                    }
                }
            }
        }
    }

    /// Never fails: an unreadable checkpoint is pushed as a `blob: Err` base that a later checkpoint or rewind marker
    /// can still supersede.
    fn handle_checkpoint(&mut self, info: &CompactionCheckpointInfo, session_dir: &Path) {
        let checkpoint_path = session_dir.join(&info.checkpoint_file);
        let compaction_at = info.prompt_index_at_compaction;

        if self.target < compaction_at {
            match read_checkpoint_file(&checkpoint_path) {
                Ok(file) => {
                    if self.original_user_info.is_none() {
                        self.original_user_info = file.original_user_info;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        compaction_at,
                        target = self.target,
                        "checkpoint unreadable; pre-compaction rewind keeps the current user_info"
                    );
                }
            }
            return;
        }

        self.in_user_message = false;
        self.current_user_text.clear();
        self.current_user_prompt_index = None;
        self.current_user_is_interjection = false;
        self.current_agent_text.clear();
        self.has_pending_agent = false;

        // Counting proceeds as if the blob loaded, so later markers and checkpoints resolve identically either way
        self.prompt_counter = compaction_at;

        match read_checkpoint_file(&checkpoint_path) {
            Ok(file) => {
                // handle_rewind needs original_user_info for the raw-updates prefix case even when the conversation is replaced
                if self.original_user_info.is_none() {
                    self.original_user_info = file.original_user_info;
                }

                self.conversation = file.compacted_history;
                // Checkpoints predate this binary's validation (or the API's current validators), so heal them like the jsonl loader does
                // Otherwise a cross-compaction rewind re-injects a stripped poison image and every turn 400s until the next restart
                let stripped_images =
                    crate::session::storage::jsonl::strip_invalid_images(&mut self.conversation);
                if stripped_images > 0 {
                    tracing::warn!(
                        count = stripped_images,
                        "stripped invalid images from compaction checkpoint history"
                    );
                }
                // The synthetic auto-continue prompt goes inside the base so neither the counter nor truncation sees it as a turn
                if let Some(ac) = &info.auto_continue {
                    self.conversation
                        .push(ConversationItem::user(ac.prompt_text.clone()));
                }
                self.bases.clear();
                self.bases.push(Base {
                    prompt_index: compaction_at,
                    base_len: self.conversation.len(),
                    blob: Ok(()),
                });

                tracing::debug!(
                    prompt_counter = self.prompt_counter,
                    "Replay: loaded compaction checkpoint"
                );
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    compaction_at,
                    "checkpoint unreadable; rewind fails unless a later checkpoint or rewind marker supersedes it"
                );
                let word = match e.kind() {
                    io::ErrorKind::NotFound => "missing",
                    io::ErrorKind::InvalidData => "corrupt",
                    io::ErrorKind::Unsupported => "unsupported",
                    _ => "unreadable",
                };
                // The TUI shows one truncated line, so the range and advice lead and the path trails
                let relative_path = &info.checkpoint_file;
                let user_error = io::Error::new(
                    e.kind(),
                    format!(
                        "checkpoint for prompts #{compaction_at} onward is {word}. \
                         Pick a prompt before #{compaction_at} or at or after the next compaction. \
                         ({relative_path})"
                    ),
                );
                // An unreadable checkpoint never replaced the conversation, so the base below it stays
                self.bases.push(Base {
                    prompt_index: compaction_at,
                    base_len: self.conversation.len(),
                    blob: Err(user_error),
                });
            }
        }
    }

    fn handle_rewind_marker(&mut self, marker_target: usize) {
        // Discard any in-progress partial messages: they belong to the timeline being discarded, so we drop them rather than flushing
        self.current_user_text.clear();
        self.current_user_prompt_index = None;
        self.current_user_is_interjection = false;
        self.current_agent_text.clear();
        self.has_pending_agent = false;
        self.in_user_message = false;

        if self.prompt_counter <= marker_target {
            return;
        }

        // `marker_target = N` means "rewind to before prompt N", keeping prompts 0..N-1 (N prompts total)
        while self
            .bases
            .last()
            .is_some_and(|base| base.prompt_index > marker_target)
        {
            self.bases.pop();
        }
        if let Some(top) = self.bases.last() {
            let (base_len, base_index) = (top.base_len, top.prompt_index);
            self.truncate_after_base(base_len, marker_target - base_index);
        } else if marker_target == 0 {
            self.conversation.clear();
        } else {
            // No base left; a popped loaded blob still heads the conversation and is truncated like raw items (lossy)
            let truncate_at = self.truncate_target(marker_target);
            let keep =
                crate::sampling::conversation_truncate_for_prompt(&self.conversation, truncate_at);
            self.conversation.truncate(keep);
        }

        self.prompt_counter = marker_target;
    }

    /// Keeps the base blob intact and only the first `turns_to_keep` real user turns appended after it.
    fn truncate_after_base(&mut self, base_len: usize, turns_to_keep: usize) {
        let mut seen_marker = false;
        let mut user_count = 0;
        let mut cut_pos = self.conversation.len();
        if let Some(tail) = self.conversation.get(base_len..) {
            for (i, item) in tail.iter().enumerate() {
                if counts_as_replay_turn_progressive(item, &mut seen_marker) {
                    user_count += 1;
                    if user_count > turns_to_keep {
                        cut_pos = base_len + i;
                        break;
                    }
                }
            }
        }
        self.conversation.truncate(cut_pos);
    }

    fn handle_user_chunk(&mut self, chunk: &agent_client_protocol::ContentChunk) {
        if crate::session::storage::is_host_turn_chunk(chunk) {
            self.flush_host_turn_boundary();
            return;
        }
        let chunk_prompt_index = chunk
            .meta
            .as_ref()
            .and_then(|m| m.get("promptIndex"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        if chunk_prompt_index.is_some() {
            self.seen_prompt_index_marker = true;
        }
        let interjection = crate::session::storage::is_interjection_chunk(chunk);
        // Each interjection's text chunk is its own item; an interjection never merges with a neighbouring prompt run
        let opens_interjection =
            interjection && matches!(chunk.content, agent_client_protocol::ContentBlock::Text(_));

        if !self.in_user_message {
            self.flush_pending_agent();
            self.in_user_message = true;
            self.current_user_text.clear();
            self.current_user_prompt_index = chunk_prompt_index;
            self.current_user_is_interjection = interjection;
        } else if (chunk_prompt_index != self.current_user_prompt_index
            && (chunk_prompt_index.is_some() || self.current_user_prompt_index.is_some()))
            || interjection != self.current_user_is_interjection
            || opens_interjection
        {
            // New run: promptIndex changed, transition between marked/unmarked, or an interjection boundary.
            self.flush_pending_user();
            self.in_user_message = true;
            self.current_user_text.clear();
            self.current_user_prompt_index = chunk_prompt_index;
            self.current_user_is_interjection = interjection;
        } else if self.current_user_prompt_index.is_none() {
            self.current_user_prompt_index = chunk_prompt_index;
        }

        // No early stop when prompt_counter > target: a later RewindMarker can reset the counter back below the target
        if let agent_client_protocol::ContentBlock::Text(t) = &chunk.content {
            self.current_user_text.push_str(&t.text);
        }
    }

    fn handle_agent_chunk(&mut self, chunk: &agent_client_protocol::ContentChunk) {
        if crate::session::storage::is_host_turn_chunk(chunk) {
            self.flush_host_turn_boundary();
            return;
        }

        // An agent chunk ends any in-progress user message.
        if self.in_user_message {
            self.flush_pending_user();
            self.in_user_message = false;
        }

        if let agent_client_protocol::ContentBlock::Text(t) = &chunk.content {
            self.current_agent_text.push_str(&t.text);
            self.has_pending_agent = true;
        }
    }

    fn flush_host_turn_boundary(&mut self) {
        if self.in_user_message {
            self.flush_pending_user();
            self.in_user_message = false;
        }
        self.flush_pending_agent();
    }

    fn conversation_has_markers(&self) -> bool {
        self.seen_prompt_index_marker
            || self
                .conversation
                .iter()
                .any(|i| matches!(i, ConversationItem::User(u) if u.prompt_index.is_some()))
    }

    /// Absolute `target` when items carry `prompt_index`; else `target - 1` for the preamble-aware counting fallback.
    fn truncate_target(&self, target: usize) -> usize {
        if self.conversation_has_markers() {
            target
        } else {
            target.saturating_sub(1)
        }
    }

    fn flush_pending_user(&mut self) {
        let interjection = std::mem::take(&mut self.current_user_is_interjection);
        if self.current_user_text.is_empty() {
            self.current_user_prompt_index = None;
            return;
        }
        let text = std::mem::take(&mut self.current_user_text);
        let pi = self.current_user_prompt_index.take();
        if interjection {
            // Tagged like the live drain's item; never a counted turn
            self.conversation.push(ConversationItem::interjection(text));
            return;
        }
        if let Some(pi) = pi {
            let mut item = ConversationItem::user(text);
            item.set_prompt_index(pi);
            self.conversation.push(item);
            self.prompt_counter += 1;
        } else if !self.seen_prompt_index_marker {
            self.conversation.push(ConversationItem::user(text));
            self.prompt_counter += 1;
        } else {
            // Mid-turn phantom after markers: keep text, do not count.
            self.conversation.push(ConversationItem::user(text));
        }
    }

    fn flush_pending_agent(&mut self) {
        if self.has_pending_agent {
            self.conversation
                .push(ConversationItem::assistant(std::mem::take(
                    &mut self.current_agent_text,
                )));
            self.has_pending_agent = false;
        }
    }
}

fn read_checkpoint_file(path: &Path) -> io::Result<CompactionCheckpointFile> {
    let bytes = std::fs::read(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => io::Error::new(
            io::ErrorKind::NotFound,
            format!("Compaction checkpoint file missing: {}", path.display()),
        ),
        kind => io::Error::new(
            kind,
            format!(
                "Compaction checkpoint file unreadable: {} ({e})",
                path.display()
            ),
        ),
    })?;
    let file: CompactionCheckpointFile = serde_json::from_slice(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Compaction checkpoint file corrupt: {} ({e})",
                path.display()
            ),
        )
    })?;
    if file.schema_version > 1 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Unsupported checkpoint schema version {} in {}",
                file.schema_version,
                path.display()
            ),
        ));
    }
    Ok(file)
}

/// Progressive post-checkpoint turn: unmarked users count until the first marker in the slice; after that only marked users count.
fn counts_as_replay_turn_progressive(item: &ConversationItem, seen_marker: &mut bool) -> bool {
    let ConversationItem::User(u) = item else {
        return false;
    };
    if u.prompt_index.is_some() {
        *seen_marker = true;
        true
    } else {
        !*seen_marker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::notification::{
        AutoContinueInfo, CompactionCheckpointFile, CompactionCheckpointInfo,
        SessionNotification as XaiNotification, SessionUpdate as XaiSessionUpdate,
    };
    use agent_client_protocol as acp;
    use tempfile::TempDir;

    fn make_user_update(session_id: &str, text: &str) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_string()),
            ))),
        )))
    }

    fn make_user_update_pi(session_id: &str, text: &str, prompt_index: usize) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            acp::SessionUpdate::UserMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                    text.to_string(),
                )))
                .meta(
                    serde_json::json!({ "promptIndex": prompt_index })
                        .as_object()
                        .cloned(),
                ),
            ),
        )))
    }

    #[test]
    fn test_replay_consecutive_prompts_no_agent_between_are_distinct_turns() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "P0", 0),
            make_user_update_pi("s1", "P1", 1),
            make_user_update_pi("s1", "P2", 2),
            make_user_update_pi("s1", "P3", 3),
            make_user_update_pi("s1", "P4", 4),
            make_user_update_pi("s1", "P5", 5),
        ];
        let result = replay_updates(&updates, tmp.path(), 3);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(
            user_msgs,
            vec!["P0", "P1", "P2"],
            "consecutive cancelled-turn prompts must be distinct turns and truncate correctly"
        );
        assert_eq!(result.prompt_index_reached, 3);
    }

    fn make_agent_update(session_id: &str, text: &str) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_string()),
            ))),
        )))
    }

    fn make_host_turn_update(session_id: &str, text: &str, user: bool) -> SessionUpdate {
        let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
            text.to_string(),
        )))
        .meta(serde_json::json!({ "hostTurn": true }).as_object().cloned());
        let update = if user {
            acp::SessionUpdate::UserMessageChunk(chunk)
        } else {
            acp::SessionUpdate::AgentMessageChunk(chunk)
        };
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            update,
        )))
    }

    #[test]
    fn test_replay_suppresses_full_host_turn_and_flushes_preceding_agent() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "real user", 0),
            make_agent_update("s1", "real assistant"),
            make_host_turn_update("s1", "/workflows", true),
            make_host_turn_update("s1", "host-only slash output", false),
            make_user_update_pi("s1", "next real user", 1),
            make_agent_update("s1", "next real assistant"),
        ];

        let result = replay_updates(&updates, tmp.path(), 2);
        let texts: Vec<_> = result
            .conversation
            .iter()
            .map(ConversationItem::text_content)
            .collect();
        assert_eq!(
            texts,
            vec![
                "real user",
                "real assistant",
                "next real user",
                "next real assistant"
            ]
        );
        assert_eq!(result.prompt_index_reached, 2);
    }

    fn make_rewind_marker(target: usize) -> SessionUpdate {
        SessionUpdate::Xai(Box::new(XaiNotification {
            session_id: acp::SessionId::new("test"),
            update: XaiSessionUpdate::RewindMarker {
                target_prompt_index: target,
                created_at: "2024-01-01T00:00:00Z".to_string(),
            },
            meta: None,
        }))
    }

    fn make_checkpoint(
        checkpoint_id: &str,
        prompt_index_at_compaction: usize,
        auto_continue: Option<AutoContinueInfo>,
    ) -> SessionUpdate {
        SessionUpdate::Xai(Box::new(XaiNotification {
            session_id: acp::SessionId::new("test"),
            update: XaiSessionUpdate::CompactionCheckpoint(Box::new(CompactionCheckpointInfo {
                checkpoint_id: checkpoint_id.to_string(),
                prompt_index_at_compaction,
                checkpoint_file: format!("compaction_checkpoints/{checkpoint_id}.json"),
                auto_continue,
                schema_version: 1,
                created_at: "2024-01-01T00:00:00Z".to_string(),
            })),
            meta: None,
        }))
    }

    fn write_checkpoint_file(
        session_dir: &Path,
        checkpoint_id: &str,
        prompt_index_at_compaction: usize,
        compacted_history: Vec<ConversationItem>,
    ) {
        let dir = session_dir.join("compaction_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        let file = CompactionCheckpointFile {
            checkpoint_id: checkpoint_id.to_string(),
            prompt_index_at_compaction,
            compacted_history,
            schema_version: 1,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            original_user_info: None,
            reread_file_paths: vec![],
        };
        let bytes = serde_json::to_vec_pretty(&file).unwrap();
        std::fs::write(dir.join(format!("{checkpoint_id}.json")), bytes).unwrap();
    }

    /// Helper: write a sequence of updates to a JSONL file and replay to a target.
    fn replay_updates(
        updates: &[SessionUpdate],
        session_dir: &Path,
        target: usize,
    ) -> ReplayResult {
        try_replay_updates(updates, session_dir, target).unwrap()
    }

    fn try_replay_updates(
        updates: &[SessionUpdate],
        session_dir: &Path,
        target: usize,
    ) -> io::Result<ReplayResult> {
        let updates_path = session_dir.join("updates.jsonl");
        let mut content = Vec::new();
        for u in updates {
            let envelope = crate::session::storage::SessionUpdateEnvelope::from_update(u).unwrap();
            let mut line = serde_json::to_vec(&envelope).unwrap();
            line.push(b'\n');
            content.extend(line);
        }
        std::fs::write(&updates_path, content).unwrap();
        replay_to_prompt(&updates_path, session_dir, target)
    }

    fn texts_of(result: &ReplayResult) -> Vec<String> {
        result
            .conversation
            .iter()
            .map(ConversationItem::text_content)
            .collect()
    }

    fn two_compactions_updates() -> Vec<SessionUpdate> {
        vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt2", 3, None),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
        ]
    }

    fn write_summary_checkpoint(session_dir: &Path, id: &str, at: usize, summary: &str) {
        write_checkpoint_file(
            session_dir,
            id,
            at,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user(summary),
            ],
        );
    }

    #[test]
    fn test_replay_missing_needed_checkpoint_errors() {
        let tmp = TempDir::new().unwrap();
        write_summary_checkpoint(tmp.path(), "ckpt1", 2, "summary1");

        let err = try_replay_updates(&two_compactions_updates(), tmp.path(), 3).unwrap_err();

        assert_eq!(io::ErrorKind::NotFound, err.kind());
        assert_eq!(
            "checkpoint for prompts #3 onward is missing. \
             Pick a prompt before #3 or at or after the next compaction. \
             (compaction_checkpoints/ckpt2.json)",
            err.to_string()
        );
    }

    #[test]
    fn test_replay_corrupt_needed_checkpoint_errors() {
        let tmp = TempDir::new().unwrap();
        write_summary_checkpoint(tmp.path(), "ckpt1", 2, "summary1");
        let corrupt = tmp.path().join("compaction_checkpoints/ckpt2.json");
        std::fs::write(&corrupt, b"{not json").unwrap();

        let err = try_replay_updates(&two_compactions_updates(), tmp.path(), 3).unwrap_err();

        assert_eq!(io::ErrorKind::InvalidData, err.kind());
        assert_eq!(
            "checkpoint for prompts #3 onward is corrupt. \
             Pick a prompt before #3 or at or after the next compaction. \
             (compaction_checkpoints/ckpt2.json)",
            err.to_string()
        );
    }

    #[test]
    fn test_replay_unsupported_schema_needed_checkpoint_errors() {
        let tmp = TempDir::new().unwrap();
        write_summary_checkpoint(tmp.path(), "ckpt1", 2, "summary1");
        let newer = CompactionCheckpointFile {
            checkpoint_id: "ckpt2".to_owned(),
            prompt_index_at_compaction: 3,
            compacted_history: vec![ConversationItem::system("sys")],
            schema_version: 2,
            created_at: "2024-01-01T00:00:00Z".to_owned(),
            original_user_info: None,
            reread_file_paths: vec![],
        };
        let path = tmp.path().join("compaction_checkpoints/ckpt2.json");
        std::fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();

        let err = try_replay_updates(&two_compactions_updates(), tmp.path(), 3).unwrap_err();

        assert_eq!(io::ErrorKind::Unsupported, err.kind());
        assert_eq!(
            "checkpoint for prompts #3 onward is unsupported. \
             Pick a prompt before #3 or at or after the next compaction. \
             (compaction_checkpoints/ckpt2.json)",
            err.to_string()
        );
    }

    #[test]
    fn test_replay_missing_later_checkpoint_pre_compaction_target_succeeds() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
        ];

        let result = try_replay_updates(&updates, tmp.path(), 1).unwrap();

        assert_eq!(vec!["P0", "R0"], texts_of(&result));
        assert_eq!(None, result.original_user_info);
        assert_eq!(None, result.last_compaction_prompt_index);
        assert_eq!(1, result.prompt_index_reached);
    }

    #[test]
    fn test_replay_rewind_marker_deactivates_missing_checkpoint() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "P0", 0),
            make_agent_update("s1", "R0"),
            make_user_update_pi("s1", "P1", 1),
            make_agent_update("s1", "R1"),
            make_user_update_pi("s1", "P2", 2),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt1", 3, None),
            make_user_update_pi("s1", "P3", 3),
            make_agent_update("s1", "R3"),
            make_rewind_marker(1),
            make_user_update_pi("s1", "P1_prime", 1),
            make_agent_update("s1", "R1_prime"),
            make_user_update_pi("s1", "P2_prime", 2),
            make_agent_update("s1", "R2_prime"),
        ];

        let result = try_replay_updates(&updates, tmp.path(), 3).unwrap();

        assert_eq!(
            vec!["P0", "R0", "P1_prime", "R1_prime", "P2_prime", "R2_prime"],
            texts_of(&result)
        );
        assert_eq!(None, result.last_compaction_prompt_index);
        assert_eq!(None, result.original_user_info);
        assert_eq!(3, result.prompt_index_reached);
    }

    /// The marker pops the unreadable ckpt2 and the loaded ckpt1 below it becomes the base again.
    #[test]
    fn test_replay_marker_between_loaded_and_unreadable_checkpoints_keeps_loaded_base() {
        let tmp = TempDir::new().unwrap();
        write_summary_checkpoint(tmp.path(), "ckpt1", 2, "summary1");
        let updates = vec![
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
            make_checkpoint("ckpt2", 4, None),
            make_user_update("s1", "P4"),
            make_agent_update("s1", "R4"),
            make_rewind_marker(3),
            make_user_update("s1", "P3_prime"),
            make_agent_update("s1", "R3_prime"),
        ];

        let result = try_replay_updates(&updates, tmp.path(), 4).unwrap();

        assert_eq!(
            vec!["sys", "summary1", "P2", "R2", "P3_prime", "R3_prime"],
            texts_of(&result)
        );
        assert_eq!(Some(2), result.last_compaction_prompt_index);
        assert_eq!(4, result.prompt_index_reached);
    }

    /// Both checkpoints are unreadable; the marker pops ckpt2 and the remaining base ckpt1 still fails the replay.
    #[test]
    fn test_replay_marker_discards_unreadable_checkpoint_above_unreadable_base() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
            make_checkpoint("ckpt2", 4, None),
            make_user_update("s1", "P4"),
            make_agent_update("s1", "R4"),
            make_rewind_marker(3),
        ];

        let err = try_replay_updates(&updates, tmp.path(), 4).unwrap_err();

        assert_eq!(io::ErrorKind::NotFound, err.kind());
        assert_eq!(
            "checkpoint for prompts #2 onward is missing. \
             Pick a prompt before #2 or at or after the next compaction. \
             (compaction_checkpoints/ckpt1.json)",
            err.to_string()
        );
    }

    /// P1 has no agent reply before ckpt1, so both runs must discard it rather than count it.
    #[test]
    fn test_replay_unreadable_superseded_checkpoint_matches_loaded_replay() {
        let updates = vec![
            make_user_update_pi("s1", "P0", 0),
            make_agent_update("s1", "R0"),
            make_user_update_pi("s1", "P1", 1),
            make_checkpoint("ckpt1", 2, None),
            make_user_update_pi("s1", "P2", 2),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt2", 3, None),
            make_user_update_pi("s1", "P3", 3),
            make_agent_update("s1", "R3"),
        ];

        let both_present = TempDir::new().unwrap();
        write_summary_checkpoint(both_present.path(), "ckpt1", 2, "summary1");
        write_summary_checkpoint(both_present.path(), "ckpt2", 3, "summary2");
        let loaded = try_replay_updates(&updates, both_present.path(), 4).unwrap();

        let first_missing = TempDir::new().unwrap();
        write_summary_checkpoint(first_missing.path(), "ckpt2", 3, "summary2");
        let unreadable = try_replay_updates(&updates, first_missing.path(), 4).unwrap();

        assert_eq!(vec!["sys", "summary2", "P3", "R3"], texts_of(&loaded));
        assert_eq!(texts_of(&loaded), texts_of(&unreadable));
        assert_eq!(4, loaded.prompt_index_reached);
        assert_eq!(loaded.prompt_index_reached, unreadable.prompt_index_reached);
        assert_eq!(
            loaded.last_compaction_prompt_index,
            unreadable.last_compaction_prompt_index
        );
    }

    /// A persisted interjection chunk as the shell writes it: framed text plus the `interjection` flag.
    fn make_interjection_update(session_id: &str, typed: &str) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            acp::SessionUpdate::UserMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                    distill_interjection_core::format_interjection(typed.to_string()),
                )))
                .meta(
                    serde_json::json!({ crate::session::storage::INTERJECTION_META_KEY: true })
                        .as_object()
                        .cloned(),
                ),
            ),
        )))
    }

    /// Interjections come back tagged and framed as the live drain pushed them, one item each, and never
    /// merge with a neighbouring prompt run (here: no agent text between the prompt echo and the drain).
    #[test]
    fn test_replay_tags_interjections_and_keeps_them_distinct() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "P0", 0),
            make_interjection_update("s1", "first"),
            make_interjection_update("s1", "second"),
            make_agent_update("s1", "A0"),
            make_user_update_pi("s1", "P1", 1),
            make_agent_update("s1", "A1"),
        ];
        let result = replay_updates(&updates, tmp.path(), 2);
        let users: Vec<(String, bool, Option<usize>)> = result
            .conversation
            .iter()
            .filter_map(|c| match c {
                ConversationItem::User(u) => Some((
                    c.text_content(),
                    u.synthetic_reason == crate::sampling::SyntheticReason::Interjection,
                    u.prompt_index,
                )),
                _ => None,
            })
            .collect();
        let framed = |t: &str| distill_interjection_core::format_interjection(t.to_string());
        assert_eq!(
            users,
            vec![
                ("P0".to_string(), false, Some(0)),
                (framed("first"), true, None),
                (framed("second"), true, None),
                ("P1".to_string(), false, Some(1)),
            ]
        );
        assert_eq!(
            result.prompt_index_reached, 2,
            "interjections are never counted as turns"
        );
    }

    #[test]
    fn test_replay_unmarked_phantom_between_marked_prompts() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update_pi("s1", "P0", 0),
            make_agent_update("s1", "A0"),
            make_user_update("s1", "!pwd phantom"),
            make_agent_update("s1", "bash"),
            make_user_update_pi("s1", "P1", 1),
            make_agent_update("s1", "A1"),
            make_user_update_pi("s1", "P2", 2),
            make_agent_update("s1", "A2"),
        ];
        let result = replay_updates(&updates, tmp.path(), 2);
        let real: Vec<_> = result
            .conversation
            .iter()
            .filter_map(|c| match c {
                ConversationItem::User(u) if u.prompt_index.is_some() => Some(c.text_content()),
                _ => None,
            })
            .collect();
        assert_eq!(real, vec!["P0", "P1"]);
        assert!(
            result.conversation.iter().any(|c| {
                matches!(
                    c,
                    ConversationItem::User(u)
                        if u.prompt_index.is_none() && c.text_content().contains("pwd")
                )
            }),
            "phantom text kept for context"
        );
        assert_eq!(result.prompt_index_reached, 2);
    }

    #[test]
    fn test_replay_simple_no_compaction() {
        let tmp = TempDir::new().unwrap();
        let updates = vec![
            make_user_update("s1", "hello"),
            make_agent_update("s1", "hi there"),
            make_user_update("s1", "fix the bug"),
            make_agent_update("s1", "done"),
            make_user_update("s1", "add tests"),
            make_agent_update("s1", "tests added"),
        ];

        // Replay to prompt 1: keep prompts 0..0 (just "hello")
        let result = replay_updates(&updates, tmp.path(), 1);
        assert_eq!(result.prompt_index_reached, 1);
        assert_eq!(result.conversation.len(), 2);
        let texts: Vec<_> = result
            .conversation
            .iter()
            .map(|i| i.text_content())
            .collect();
        assert_eq!(texts, ["hello", "hi there"]);
    }

    #[test]
    fn test_replay_with_rewind_marker() {
        let tmp = TempDir::new().unwrap();
        // P0, P1, P2, rewind(1) removes P1 and P2, then P1'
        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_rewind_marker(1), // keep P0 only
            make_user_update("s1", "P1_prime"),
            make_agent_update("s1", "R1_prime"),
        ];

        // Replay to prompt 2: keep prompts 0..1 (P0, P1')
        let result = replay_updates(&updates, tmp.path(), 2);
        // After rewind(1): P0 kept, P1 and P2 discarded
        // P1_prime added as prompt 1, giving [P0, R0, P1_prime, R1_prime]
        assert_eq!(result.conversation.len(), 4);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(user_msgs, vec!["P0", "P1_prime"]);
    }

    #[test]
    fn test_replay_pre_compaction_target() {
        let tmp = TempDir::new().unwrap();

        // P0, P1, checkpoint(at=2), P2
        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted summary"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
        ];

        // Replay to prompt 1 (pre-compaction): should IGNORE the checkpoint
        // Keep prompts 0..0 (just P0)
        let result = replay_updates(&updates, tmp.path(), 1);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(user_msgs, vec!["P0"]);
    }

    #[test]
    fn test_replay_post_compaction_target() {
        let tmp = TempDir::new().unwrap();

        // Checkpoint replaces conversation at prompt 2
        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted summary"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
        ];

        // Replay to prompt 3 (post-compaction): keep prompts 0..2
        // Checkpoint blob and P2 only (P3 removed)
        let result = replay_updates(&updates, tmp.path(), 3);
        assert_eq!(result.conversation.len(), 4);
        let texts: Vec<_> = result
            .conversation
            .iter()
            .map(|i| i.text_content())
            .collect();
        assert_eq!(texts, ["sys", "compacted summary", "P2", "R2"]);
        assert_eq!(result.prompt_index_reached, 3);
    }

    /// A checkpoint written before this binary's validation (or before the API tightened its validators) can carry an unsendable image.
    /// The splice must heal it like the jsonl loader does, or a cross-compaction rewind re-poisons a healed session.
    #[test]
    fn test_replay_checkpoint_strips_invalid_images() {
        use base64::Engine as _;
        let tmp = TempDir::new().unwrap();

        // 16×16 icon: below the API's 512-total-pixel floor.
        let mut png = Vec::new();
        image::ImageBuffer::from_pixel(16, 16, image::Rgba([9u8, 9, 9, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let mut poisoned = ConversationItem::user("look at this icon");
        poisoned.add_image(url);

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            1,
            vec![ConversationItem::system("sys"), poisoned],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_checkpoint("ckpt1", 1, None),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
        ];

        let result = replay_updates(&updates, tmp.path(), 2);
        let Some(ConversationItem::User(u)) = result.conversation.get(1) else {
            panic!(
                "expected user item from checkpoint: {:?}",
                result.conversation
            );
        };
        assert!(
            u.content.iter().all(|p| match p {
                crate::sampling::ContentPart::Image { url } => !url.starts_with("data:"),
                _ => true,
            }),
            "below-floor image must be stripped from the checkpoint splice"
        );
    }

    /// Auto-continue prompt is synthetic (not a real user prompt) so it must NOT increment prompt_counter.
    /// It's appended to the conversation for context but the next real prompt still gets the expected index.
    #[test]
    fn test_replay_checkpoint_with_auto_continue() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint(
                "ckpt1",
                2,
                Some(AutoContinueInfo {
                    prompt_text: "Continue working".to_string(),
                }),
            ),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
        ];

        // Replay to prompt 3: keep prompts 0..2 (checkpoint blob, auto-continue, P2)
        let result = replay_updates(&updates, tmp.path(), 3);
        // checkpoint sets counter to 2, auto-continue doesn't increment, P2 increments to 3, so prompt_index_reached is 3
        assert_eq!(result.conversation.len(), 5);
        let texts: Vec<_> = result
            .conversation
            .iter()
            .map(|i| i.text_content())
            .collect();
        assert_eq!(texts, ["sys", "compacted", "Continue working", "P2", "R2"]);
        assert_eq!(result.prompt_index_reached, 3);
    }

    #[test]
    fn test_find_latest_checkpoint_none() {
        let tmp = TempDir::new().unwrap();
        let updates_path = tmp.path().join("updates.jsonl");
        std::fs::write(&updates_path, "").unwrap();

        let result = find_latest_compaction_checkpoint(&updates_path).unwrap();
        assert!(result.is_none());
    }

    /// Scenario H: rewind marker after a loaded checkpoint.
    /// checkpoint(at=2), P2, P3, RewindMarker(2), P2'
    /// Replaying to prompt 2 should give checkpoint and P2' (not P2 or P3).
    #[test]
    fn test_replay_rewind_marker_after_checkpoint() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("compacted summary"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
            make_rewind_marker(2),
            make_user_update("s1", "P2_prime"),
            make_agent_update("s1", "R2_prime"),
        ];

        // Replay to prompt 3: keep 0..2 (checkpoint and P2', after the rewind marker)
        let result = replay_updates(&updates, tmp.path(), 3);

        // The checkpoint blob has 2 items (system and user summary)
        // After the rewind marker discards P2 and P3, P2' is added
        // Result: [sys, summary, P2_prime, R2_prime]
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();

        // The compacted summary is a synthetic User msg inside the checkpoint blob.
        // P2_prime is the real user msg appended after.
        assert!(
            user_msgs.contains(&"P2_prime".to_string()),
            "Should contain P2_prime, got: {:?}",
            user_msgs
        );
        assert!(
            !user_msgs.contains(&"P2".to_string()),
            "Should NOT contain old P2, got: {:?}",
            user_msgs
        );
        assert!(
            !user_msgs.contains(&"P3".to_string()),
            "Should NOT contain P3, got: {:?}",
            user_msgs
        );
    }

    /// Scenario E: multiple compactions, rewind to before the first.
    /// P0, P1, checkpoint#1(at=2), P2, checkpoint#2(at=3), P3
    /// Rewind to P1 should ignore both checkpoints.
    #[test]
    fn test_replay_multiple_compactions_rewind_to_before_first() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary1"),
            ],
        );
        write_checkpoint_file(
            tmp.path(),
            "ckpt2",
            3,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary2"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt2", 3, None),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
        ];

        // Rewind to P1: both checkpoints should be ignored
        // Keep prompts 0..0 (just P0)
        let result = replay_updates(&updates, tmp.path(), 1);
        assert_eq!(result.conversation.len(), 2);
        let user_msgs: Vec<String> = result
            .conversation
            .iter()
            .filter(|c| matches!(c, ConversationItem::User(_)))
            .map(|c| c.text_content())
            .collect();
        assert_eq!(user_msgs, vec!["P0"]);
    }

    /// Scenario E variant: rewind to between two compactions.
    /// Should use checkpoint#1 and replay P2.
    #[test]
    fn test_replay_multiple_compactions_rewind_between() {
        let tmp = TempDir::new().unwrap();

        write_checkpoint_file(
            tmp.path(),
            "ckpt1",
            2,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary1"),
            ],
        );
        write_checkpoint_file(
            tmp.path(),
            "ckpt2",
            3,
            vec![
                ConversationItem::system("sys"),
                ConversationItem::user("summary2"),
            ],
        );

        let updates = vec![
            make_user_update("s1", "P0"),
            make_agent_update("s1", "R0"),
            make_user_update("s1", "P1"),
            make_agent_update("s1", "R1"),
            make_checkpoint("ckpt1", 2, None),
            make_user_update("s1", "P2"),
            make_agent_update("s1", "R2"),
            make_checkpoint("ckpt2", 3, None),
            make_user_update("s1", "P3"),
            make_agent_update("s1", "R3"),
        ];

        // Replay to prompt 3: keep prompts 0..2 via ckpt1 ckpt1 loaded (target 3 >= 2), ckpt2 also loaded (target 3 >= 3).
        // ckpt2 replaces ckpt1. Then P3 is the first post-ckpt2 prompt.
        // Keep 3 - 3 = 0 post-ckpt2 prompts, so just the ckpt2 blob.
        let result = replay_updates(&updates, tmp.path(), 3);
        assert_eq!(result.conversation.len(), 2);
        let texts: Vec<_> = result
            .conversation
            .iter()
            .map(|i| i.text_content())
            .collect();
        assert_eq!(texts, ["sys", "summary2"]);
    }
}
