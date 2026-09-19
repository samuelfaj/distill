// Modified for Distill by Samuel Fajreldines, 2026.
use async_trait::async_trait;
use std::io::{self, BufRead, BufReader, Seek, Write};
use std::path::{Path, PathBuf};

use crate::extensions::notification::SessionNotification;
use crate::sampling::ConversationItem;
use crate::session::info::Info;
use crate::session::persistence::Summary;
use crate::session::signals::SessionSignals;
use crate::session::wire_tags::{REWIND_MARKER, USER_MESSAGE_CHUNK};
use crate::tools::todo::TodoState;
use agent_client_protocol as acp;
use distill_sampling_types::ReasoningEffort;
use distill_workspace::session::file_state::RewindPoint;

pub mod jsonl;
pub(crate) mod relocation;
mod replay;
#[cfg(test)]
mod replay_tests;
pub mod search;
mod search_content;
pub(crate) mod summary_write;

/// The session search index moved to its own crate; re-exported here so `session::storage::search_fts::…` keeps resolving for its consumers.
pub use distill_session_search::fts as search_fts;

/// On-disk file names, relative to a session directory.
/// Single source of truth for the storage adapter and the session/state and session/import extensions.
pub(crate) const SUMMARY_FILE: &str = "summary.json";
pub(crate) const PLAN_FILE: &str = "plan.json";
pub(crate) const PLAN_MODE_FILE: &str = "plan_mode.json";
pub(crate) const SIGNALS_FILE: &str = "signals.json";
pub(crate) const USAGE_FILE: &str = "usage.json";
pub(crate) const GOAL_STATE_FILE: &str = "goal/state.json";
pub(crate) const ANNOUNCEMENT_STATE_FILE: &str = "announcement_state.json";
pub(crate) const CHAT_HISTORY_FILE: &str = "chat_history.jsonl";
pub(crate) const UPDATES_FILE: &str = "updates.jsonl";

/// Write `bytes` to `path` by writing a uniquely named sibling temp file and renaming it over the target.
/// A crash or a concurrent writer never leaves a torn file; the temp is removed on failure.
pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_bytes_atomic_with(path, bytes, sync_file_durable, || {
        sync_parent_dir_durable(path)
    })
}

fn write_bytes_atomic_with(
    path: &Path,
    bytes: &[u8],
    sync_file: impl Fn(&std::fs::File) -> io::Result<()>,
    sync_parent: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    let tmp = temp_sibling(path);
    let write_synced = || -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        // NTFS/ext4 journal a rename as old-file-or-new-file only when the new file's data is already flushed
        // Without this fsync a power loss can leave the committed rename in place with zero-length content
        sync_file(&file)
    };
    // Old-or-new only covers replacing a file, whose direntry is already durable A first-time create (a session's first summary.json) has no old file.
    // Its new entry can vanish on power loss until the parent directory is synced A retry after a rename whose parent sync failed sees the file present and cannot tell create from replace.
    // So every successful rename pays the parent sync.
    match write_synced().and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => sync_parent(),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn sync_file_durable(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    file.sync_all()?;
    fullfsync_raw(file.as_raw_fd())
}

#[cfg(target_os = "macos")]
pub(crate) fn fullfsync_raw(fd: std::os::fd::RawFd) -> io::Result<()> {
    // macOS fsync may stop at volatile drive caches; F_FULLFSYNC requests stable media.
    if unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(all(unix, not(target_os = "macos")), windows))]
pub(crate) fn sync_file_durable(file: &std::fs::File) -> io::Result<()> {
    file.sync_all()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_file_durable(_file: &std::fs::File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "durable file sync is unsupported on this platform",
    ))
}

/// Fsync `dir` so entries just created or renamed into it survive power loss.
#[cfg(target_os = "macos")]
pub(crate) fn sync_dir_durable(dir: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let file = std::fs::File::open(dir)?;
    file.sync_all()?;
    // Match file durability: macOS fsync may stop at volatile drive caches.
    fullfsync_raw(file.as_raw_fd())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn sync_dir_durable(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_dir_durable(_dir: &Path) -> io::Result<()> {
    // Windows has no supported directory-handle fsync
    // NTFS journals directory metadata, which can roll a very recent create back to absent but never to garbage
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_dir_durable(_dir: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "durable directory sync is unsupported on this platform",
    ))
}

/// Fsync `<cwd_dir>/.cwd` if present.
/// Hash-encoded path recovery must not be frozen to a torn marker when a later parent-dir sync makes its direntry durable.
/// Open write-capable: Windows `FlushFileBuffers` on a read-only handle cannot persist the bytes.
pub(crate) fn sync_cwd_marker_if_present(cwd_dir: &Path) -> io::Result<()> {
    sync_cwd_marker_if_present_with(cwd_dir, sync_file_durable)
}

pub(crate) fn sync_cwd_marker_if_present_with(
    cwd_dir: &Path,
    sync_file: impl Fn(&std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let cwd_file = cwd_dir.join(".cwd");
    match std::fs::OpenOptions::new().write(true).open(&cwd_file) {
        Ok(file) => sync_file(&file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub(crate) fn sync_parent_dir_durable(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    sync_dir_durable(parent)
}

/// Run `create` (a `create_dir_all`-style creation of `dir`), then [`sync_dir_durable`] every directory that gained a new entry.
/// The created chain itself then survives power loss; fsyncing a file only makes its own direntry durable, not the directories above it.
/// The ancestors are snapshotted before `create` because afterwards the whole chain exists; an already-existing occupied chain pays no sync.
pub(crate) fn create_dir_all_durable(
    dir: &Path,
    create: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    create_dir_all_durable_with(dir, create, sync_dir_durable)
}

pub(crate) fn create_dir_all_durable_with(
    dir: &Path,
    create: impl FnOnce(&Path) -> io::Result<()>,
    sync_dir: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut gaining_an_entry = Vec::new();
    let mut cursor = dir;
    while !cursor.exists() {
        let Some(parent) = cursor.parent() else { break };
        gaining_an_entry.push(parent);
        cursor = parent;
    }
    create(dir)?;
    // A retry after a create whose parent sync failed sees the whole chain present and would otherwise sync nothing.
    // Re-sync a bounded ancestor list so the new direntry is durable, but only when `dir` is still empty.
    // `init_session` calls this on every open.
    let retry_ancestors;
    let to_sync: &[&Path] = if !gaining_an_entry.is_empty() {
        &gaining_an_entry
    } else if std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        retry_ancestors = dir
            .ancestors()
            .skip(1)
            .take(4)
            .filter(|parent| !is_fs_root(parent))
            .collect::<Vec<_>>();
        &retry_ancestors
    } else {
        return Ok(());
    };
    // Fresh creates under a new top-level directory include `/` in `gaining_an_entry`; skip it here so both paths honor the root rule
    for parent in to_sync {
        if is_fs_root(parent) {
            continue;
        }
        sync_dir(parent)?;
    }
    Ok(())
}

fn is_fs_root(path: &Path) -> bool {
    path.parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
}

pub(crate) async fn write_bytes_atomic_async(path: &Path, bytes: Vec<u8>) -> io::Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || write_bytes_atomic(&path, &bytes))
        .await
        .map_err(io::Error::other)?
}

fn to_jsonl_bytes<T: serde::Serialize>(items: &[T]) -> io::Result<Vec<u8>> {
    let mut content = Vec::new();
    for item in items {
        serde_json::to_writer(&mut content, item)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        content.push(b'\n');
    }
    Ok(content)
}

pub(crate) fn write_jsonl_atomic<T: serde::Serialize>(path: &Path, items: &[T]) -> io::Result<()> {
    write_bytes_atomic(path, &to_jsonl_bytes(items)?)
}

pub(crate) async fn write_jsonl_atomic_async<T: serde::Serialize>(
    path: &Path,
    items: &[T],
) -> io::Result<()> {
    write_bytes_atomic_async(path, to_jsonl_bytes(items)?).await
}

/// A unique sibling temp path, e.g. `summary.json` becomes `summary.json.<uuid>.tmp`.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.tmp", uuid::Uuid::now_v7()));
    PathBuf::from(name)
}

/// Rebuild the derived `chat_history.jsonl` cache from `updates.jsonl`, the durable source of truth.
/// A session then restores from its update stream alone.
pub(crate) mod chat_rebuild {
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::path::Path;

    use agent_client_protocol as acp;

    use super::{CHAT_HISTORY_FILE, SessionUpdate, UPDATES_FILE, UpdatesIterator};
    use crate::sampling::{
        AssistantItem, ContentPart, ConversationItem, SyntheticReason, ToolCall, UserItem,
    };

    /// Rebuild `chat_history.jsonl` from `updates.jsonl` alone. Builds a temp file and renames it over the target.
    /// A failed rebuild leaves the existing cache intact rather than a truncated partial that load would trust.
    pub(crate) fn rebuild_chat_history(dir: &Path) -> io::Result<usize> {
        use std::io::{Seek, Write};

        let updates_path = dir.join(UPDATES_FILE);
        let Some(iter) = UpdatesIterator::open(&updates_path)? else {
            return Ok(0);
        };

        let chat_path = dir.join(CHAT_HISTORY_FILE);
        let tmp_path = dir.join(format!("{CHAT_HISTORY_FILE}.{}.tmp", uuid::Uuid::now_v7()));
        let file = std::fs::File::create(&tmp_path)?;
        let mut writer = std::io::BufWriter::new(file);
        let mut reducer = ChatReducer::new();

        for result in iter {
            let update = match result {
                Ok(u) => u,
                Err(_) => continue,
            };

            for item in reducer.process(&update) {
                if let Ok(line) = serde_json::to_string(&item) {
                    let _ = writer.write_all(line.as_bytes());
                    let _ = writer.write_all(b"\n");
                }
            }

            // CompactionCheckpoint: truncate file and reset
            if reducer.should_truncate() {
                reducer.clear_truncate_flag();
                let _ = writer.seek(std::io::SeekFrom::Start(0));
                let _ = writer.get_mut().set_len(0);
            }
        }

        for item in reducer.flush() {
            if let Ok(line) = serde_json::to_string(&item) {
                let _ = writer.write_all(line.as_bytes());
                let _ = writer.write_all(b"\n");
            }
        }

        if let Err(e) = writer.flush() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        drop(writer);
        if let Err(e) = std::fs::rename(&tmp_path, &chat_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(reducer.count())
    }

    /// Turn boundaries: a switch from user to agent flushes the user item, and a switch from agent to user flushes the agent item.
    /// Tool completion flushes the agent item before emitting the result.
    struct ChatReducer {
        user_parts: Vec<ContentPart>,
        user_is_interjection: bool,
        agent_text: String,
        agent_tool_calls: Vec<ToolCall>,

        in_user_turn: bool,
        has_agent_content: bool,
        needs_truncate: bool,

        tool_args: HashMap<String, String>,
        emitted_tool_results: HashSet<String>,
        item_count: usize,
    }

    impl ChatReducer {
        fn new() -> Self {
            Self {
                user_parts: Vec::new(),
                user_is_interjection: false,
                agent_text: String::new(),
                agent_tool_calls: Vec::new(),
                in_user_turn: false,
                has_agent_content: false,
                needs_truncate: false,
                tool_args: HashMap::new(),
                emitted_tool_results: HashSet::new(),
                item_count: 0,
            }
        }

        fn process(&mut self, update: &SessionUpdate) -> Vec<ConversationItem> {
            match update {
                SessionUpdate::Acp(n) => self.handle_acp(&n.update),
                SessionUpdate::Xai(n) => self.handle_xai(&n.update),
            }
        }

        fn handle_acp(&mut self, update: &acp::SessionUpdate) -> Vec<ConversationItem> {
            match update {
                acp::SessionUpdate::UserMessageChunk(chunk) => self.on_user_chunk(chunk),
                acp::SessionUpdate::AgentMessageChunk(chunk) => self.on_agent_chunk(chunk),
                acp::SessionUpdate::ToolCall(tc) => self.on_tool_call(tc),
                acp::SessionUpdate::ToolCallUpdate(tc) => self.on_tool_call_update(tc),
                _ => Vec::new(), // AgentThoughtChunk, Retry, Plan not needed
            }
        }

        fn handle_xai(
            &mut self,
            update: &crate::extensions::notification::SessionUpdate,
        ) -> Vec<ConversationItem> {
            use crate::extensions::notification::SessionUpdate as XaiUpdate;

            match update {
                XaiUpdate::CompactionCheckpoint(_) => {
                    self.reset();
                    self.needs_truncate = true;
                    Vec::new()
                }
                _ => Vec::new(), // DiffReview, MemoryFlush, etc. not needed
            }
        }

        fn on_user_chunk(&mut self, chunk: &acp::ContentChunk) -> Vec<ConversationItem> {
            if super::is_host_turn_chunk(chunk) {
                return self.flush_host_turn_boundary();
            }
            let mut out = Vec::new();

            if !self.in_user_turn {
                out.extend(self.flush_agent());
                self.in_user_turn = true;
            }
            // Interjections never merge with an adjacent prompt run (tool-only first response, drain right after the
            // echo), and each interjection's text chunk opens its own item, as the live drain pushed them
            let interjection = super::is_interjection_chunk(chunk);
            let opens_interjection =
                interjection && matches!(chunk.content, acp::ContentBlock::Text(_));
            if interjection != self.user_is_interjection || opens_interjection {
                out.extend(self.flush_user());
            }
            self.user_is_interjection = interjection;

            match &chunk.content {
                acp::ContentBlock::Text(t) => {
                    self.user_parts.push(ContentPart::Text {
                        text: std::sync::Arc::<str>::from(t.text.clone()),
                    });
                }
                acp::ContentBlock::Image(img) => {
                    if let Some(uri) = &img.uri {
                        self.user_parts.push(ContentPart::Image {
                            url: std::sync::Arc::<str>::from(uri.clone()),
                        });
                    }
                }
                _ => {} // Audio, Resource, etc. not needed for chat replay
            }

            out
        }

        fn on_agent_chunk(&mut self, chunk: &acp::ContentChunk) -> Vec<ConversationItem> {
            if super::is_host_turn_chunk(chunk) {
                return self.flush_host_turn_boundary();
            }
            let mut out = Vec::new();

            if self.in_user_turn {
                out.extend(self.flush_user());
                self.in_user_turn = false;
            }

            if let acp::ContentBlock::Text(t) = &chunk.content {
                self.agent_text.push_str(&t.text);
                self.has_agent_content = true;
            }

            out
        }

        fn flush_host_turn_boundary(&mut self) -> Vec<ConversationItem> {
            let mut out = Vec::new();
            if self.in_user_turn {
                out.extend(self.flush_user());
                self.in_user_turn = false;
            }
            out.extend(self.flush_agent());
            out
        }

        fn on_tool_call(&mut self, tc: &acp::ToolCall) -> Vec<ConversationItem> {
            let id = tc.tool_call_id.0.to_string();
            let args = tc
                .raw_input
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();

            self.tool_args.insert(id.clone(), args.clone());
            self.agent_tool_calls.push(ToolCall {
                id: std::sync::Arc::<str>::from(id),
                name: tc.title.clone(),
                arguments: std::sync::Arc::<str>::from(args),
            });

            Vec::new()
        }

        fn on_tool_call_update(&mut self, tc: &acp::ToolCallUpdate) -> Vec<ConversationItem> {
            let id = tc.tool_call_id.0.to_string();
            self.maybe_backfill_args(&id, &tc.fields);

            if Self::is_completed(&tc.fields) && self.emitted_tool_results.insert(id.clone()) {
                return self.emit_tool_result(&id, &tc.fields);
            }
            Vec::new()
        }

        fn maybe_backfill_args(&mut self, id: &str, fields: &acp::ToolCallUpdateFields) {
            let Some(raw) = &fields.raw_input else { return };
            let needs_backfill = self.tool_args.get(id).is_none_or(String::is_empty);
            if !needs_backfill {
                return;
            }

            let args = raw.to_string();
            self.tool_args.insert(id.to_string(), args.clone());

            if let Some(call) = self
                .agent_tool_calls
                .iter_mut()
                .find(|c| c.id.as_ref() == id)
            {
                call.arguments = std::sync::Arc::<str>::from(args);
            }
        }

        fn is_completed(fields: &acp::ToolCallUpdateFields) -> bool {
            matches!(
                fields.status,
                Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
            )
        }

        fn emit_tool_result(
            &mut self,
            id: &str,
            fields: &acp::ToolCallUpdateFields,
        ) -> Vec<ConversationItem> {
            let mut out = Vec::new();
            out.extend(self.flush_agent());

            let content = extract_tool_result_text(fields);
            let item = ConversationItem::tool_result(id.to_string(), content);
            self.item_count += 1;
            out.push(item);
            out
        }

        fn flush_user(&mut self) -> Option<ConversationItem> {
            let interjection = std::mem::take(&mut self.user_is_interjection);
            if self.user_parts.is_empty() {
                return None;
            }
            let content = std::mem::take(&mut self.user_parts);
            let synthetic_reason = if interjection {
                SyntheticReason::Interjection
            } else {
                SyntheticReason::Human
            };
            let item = ConversationItem::User(UserItem {
                content,
                synthetic_reason,
                cwd_generation: None,
                prior_turn_interrupt: None,
                prompt_index: None,
            });
            self.item_count += 1;
            Some(item)
        }

        fn flush_agent(&mut self) -> Option<ConversationItem> {
            if !self.has_agent_content && self.agent_tool_calls.is_empty() {
                return None;
            }
            let item = ConversationItem::Assistant(AssistantItem {
                content: std::sync::Arc::<str>::from(std::mem::take(&mut self.agent_text)),
                tool_calls: std::mem::take(&mut self.agent_tool_calls),
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            });
            self.has_agent_content = false;
            self.item_count += 1;
            Some(item)
        }

        fn flush(&mut self) -> Vec<ConversationItem> {
            let mut out = Vec::new();
            out.extend(self.flush_user());
            out.extend(self.flush_agent());
            out
        }

        fn reset(&mut self) {
            self.user_parts.clear();
            self.user_is_interjection = false;
            self.agent_text.clear();
            self.agent_tool_calls.clear();
            self.tool_args.clear();
            self.emitted_tool_results.clear();
            self.in_user_turn = false;
            self.has_agent_content = false;
            self.item_count = 0;
        }

        fn should_truncate(&self) -> bool {
            self.needs_truncate
        }

        fn clear_truncate_flag(&mut self) {
            self.needs_truncate = false;
        }

        fn count(&self) -> usize {
            self.item_count
        }
    }

    /// Extract displayable text from a completed ToolCallUpdate.
    fn extract_tool_result_text(fields: &acp::ToolCallUpdateFields) -> String {
        if let Some(content) = &fields.content {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    acp::ToolCallContent::Content(acp::Content {
                        content: acp::ContentBlock::Text(t),
                        ..
                    }) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                return text;
            }
        }
        if let Some(raw) = &fields.raw_output {
            return raw.to_string();
        }
        String::new()
    }
}

/// Iterator that streams session updates from a JSONL file without loading all into memory.
pub struct UpdatesIterator {
    reader: BufReader<std::fs::File>,
    line_buffer: String,
}

impl UpdatesIterator {
    /// Returns None if the file doesn't exist.
    pub fn open(path: &Path) -> io::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        Ok(Some(Self {
            reader: BufReader::new(file),
            line_buffer: String::new(),
        }))
    }

    /// After iterating, the position is the offset of the next unread byte (i.e., EOF if all updates were consumed).
    /// Used to record the replay end offset for subsequent delta replay.
    pub fn stream_position(&mut self) -> io::Result<u64> {
        self.reader.stream_position()
    }
}

impl Iterator for UpdatesIterator {
    type Item = io::Result<SessionUpdate>;

    fn next(&mut self) -> Option<Self::Item> {
        self.line_buffer.clear();
        match self.reader.read_line(&mut self.line_buffer) {
            Ok(0) => None, // EOF
            Ok(_) => {
                let line = self.line_buffer.trim();
                if line.is_empty() {
                    return self.next();
                }
                match SessionUpdateEnvelope::from_str(line) {
                    Ok(update) => Some(Ok(update)),
                    Err(e) => Some(Err(io::Error::new(io::ErrorKind::InvalidData, e))),
                }
            }
            Err(e) => Some(Err(e)),
        }
    }
}

const ACP_SESSION_UPDATE_METHOD: &str = "session/update";

pub(crate) const XAI_SESSION_UPDATE_METHOD: &str = "_x.ai/session/update";

/// One type for both notification kinds, so all session updates can be stored in chronological order.
/// The `Serialize` implementation produces a format without timestamp (for GCS uploads, etc.).
/// For disk storage with timestamps, use `SessionUpdateEnvelope` via the JSONL adapter methods.
#[derive(Debug, Clone)]
pub enum SessionUpdate {
    /// Standard ACP session/update notification (boxed due to large size)
    Acp(Box<acp::SessionNotification>),
    /// Grok extension session notification (e.g., diff_review)
    Xai(Box<SessionNotification>),
}

impl serde::Serialize for SessionUpdate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(2))?;
        match self {
            SessionUpdate::Acp(notification) => {
                map.serialize_entry("method", ACP_SESSION_UPDATE_METHOD)?;
                map.serialize_entry("params", notification)?;
            }
            SessionUpdate::Xai(notification) => {
                map.serialize_entry("method", XAI_SESSION_UPDATE_METHOD)?;
                map.serialize_entry("params", &*notification.without_live_agent_address())?;
            }
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for SessionUpdate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize to a JSON value first to handle both envelope and legacy formats
        let value = serde_json::Value::deserialize(deserializer)?;
        SessionUpdateEnvelope::from_value(value).map_err(serde::de::Error::custom)
    }
}

/// This is the typed structure that gets written to updates.jsonl (disk storage only).
/// It is separate from `SessionUpdate`'s own serialization so other consumers (e.g., network listeners) don't see the timestamp metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionUpdateEnvelope {
    /// Unix timestamp (seconds since epoch) when this update was written.
    /// Useful for debugging timing issues in the updates.jsonl file.
    #[serde(default)]
    pub timestamp: u64,
    /// Either "session/update" for ACP or "_x.ai/session/update" for Grok extensions.
    pub method: String,
    pub params: serde_json::Value,
}

impl SessionUpdateEnvelope {
    /// Create a new envelope with the current timestamp for disk storage.
    pub(crate) fn from_update(update: &SessionUpdate) -> Result<Self, serde_json::Error> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        match update {
            SessionUpdate::Acp(notification) => Ok(Self {
                timestamp,
                method: ACP_SESSION_UPDATE_METHOD.to_string(),
                params: serde_json::to_value(notification)?,
            }),
            SessionUpdate::Xai(notification) => Ok(Self {
                timestamp,
                method: XAI_SESSION_UPDATE_METHOD.to_string(),
                params: notification.to_durable_value()?,
            }),
        }
    }

    pub(crate) fn into_update(self) -> Result<SessionUpdate, serde_json::Error> {
        if self.method == XAI_SESSION_UPDATE_METHOD {
            let notification: SessionNotification = serde_json::from_value(self.params)?;
            Ok(SessionUpdate::Xai(Box::new(notification)))
        } else {
            // ACP notification (method == "session/update" or unknown)
            let notification: acp::SessionNotification = serde_json::from_value(self.params)?;
            Ok(SessionUpdate::Acp(Box::new(notification)))
        }
    }

    /// Try to parse from a JSON value, handling both envelope format and legacy raw format.
    pub(crate) fn from_value(value: serde_json::Value) -> Result<SessionUpdate, serde_json::Error> {
        if value.get("method").is_some() {
            let envelope: SessionUpdateEnvelope = serde_json::from_value(value)?;
            envelope.into_update()
        } else {
            // Backwards compatibility: old format without envelope wrapper
            let notification: acp::SessionNotification = serde_json::from_value(value)?;
            Ok(SessionUpdate::Acp(Box::new(notification)))
        }
    }

    /// Parse a session update directly from a JSON string, avoiding intermediate `Value` allocation.
    pub(crate) fn from_str(line: &str) -> Result<SessionUpdate, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct BorrowedEnvelope<'a> {
            #[serde(default)]
            method: Option<&'a str>,
            #[serde(borrow)]
            params: &'a serde_json::value::RawValue,
        }

        if let Ok(envelope) = serde_json::from_str::<BorrowedEnvelope<'_>>(line) {
            let raw_params = envelope.params.get();
            return if envelope.method == Some(XAI_SESSION_UPDATE_METHOD) {
                let notification: SessionNotification = serde_json::from_str(raw_params)?;
                Ok(SessionUpdate::Xai(Box::new(notification)))
            } else {
                let notification: acp::SessionNotification = serde_json::from_str(raw_params)?;
                Ok(SessionUpdate::Acp(Box::new(notification)))
            };
        }

        // Backwards compatibility: legacy format without envelope
        let notification: acp::SessionNotification = serde_json::from_str(line)?;
        Ok(SessionUpdate::Acp(Box::new(notification)))
    }
}

/// All persisted data for a session
#[derive(Debug, Clone)]
pub struct PersistedData {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    /// All session updates (ACP updates and Grok extension updates) in chronological order
    pub updates: Vec<SessionUpdate>,
    pub plan_state: Option<TodoState>,
    /// Persisted plan mode lifecycle state (None for sessions created before plan mode)
    pub plan_mode_state: Option<crate::session::plan_mode::PlanModeSnapshot>,
    pub rewind_points: Vec<RewindPoint>,
    /// Persisted session signals (None for sessions created before signals persistence)
    pub signals: Option<SessionSignals>,
    /// Persisted announcement tracking state (None for sessions before this feature)
    pub announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    /// Persisted goal mode orchestration state (None for sessions without goal mode)
    pub goal_mode_state: Option<crate::session::goal_tracker::GoalOrchestration>,
    pub workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
}

/// Persisted data WITHOUT updates, for memory-efficient session loading
#[derive(Debug, Clone)]
pub struct PersistedDataLight {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    pub plan_state: Option<TodoState>,
    pub plan_mode_state: Option<crate::session::plan_mode::PlanModeSnapshot>,
    // No `rewind_points` field: the resume path defers them (loaded lazily by `FileStateTracker`)
    // Use `load_session` for the eager set
    /// Persisted session signals (None for sessions created before signals persistence)
    pub signals: Option<SessionSignals>,
    /// Persisted announcement tracking state (None for sessions before this feature)
    pub announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    /// Persisted goal mode orchestration state (None for sessions without goal mode)
    pub goal_mode_state: Option<crate::session::goal_tracker::GoalOrchestration>,
    pub workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
}

#[derive(Debug, Clone)]
pub struct CopySessionResult {
    pub chat_messages_copied: usize,
    /// Logical agent minted into the fork summary, if any.
    pub agent_id: Option<String>,
    pub updates_copied: usize,
    pub plan_state_copied: bool,
    /// Whether `plan_mode.json` (plan mode lifecycle state) was copied.
    pub plan_mode_state_copied: bool,
    pub signals_copied: bool,
    /// Whether `tool_state.json` (persisted tool state, e.g. TodoState) was copied.
    pub tool_state_copied: bool,
    /// Whether `announcement_state.json` was copied.
    pub announcement_state_copied: bool,
    /// Number of `compaction/segment_*.md` (and `INDEX.md`) files copied from the source session's compaction archive.
    /// `0` when disabled or none exist.
    pub compaction_segments_copied: usize,
    /// Number of `compaction_checkpoints/{uuid}.json` files copied for the checkpoint records retained in the copied updates.
    /// `0` when no records survive the copy or their files are missing from the source.
    pub compaction_checkpoints_copied: usize,
}

/// Options for copying session data during fork
#[derive(Debug, Clone)]
pub struct CopySessionOptions {
    /// Root-session forks mint a durable identity during the copy.
    /// Generic and subagent copies leave identity ownership to their caller.
    pub mint_session_identity: bool,
    /// Parent session ID to set in the forked session's summary.
    pub parent_session_id: Option<String>,
    /// Model ID override for the forked session (None keeps the source model).
    pub new_model_id: Option<String>,
    /// Truncate copied history to this prompt index (0-based, inclusive).
    pub target_prompt_index: Option<usize>,
    /// When true, skip `transform_conversation_cwd` during copy.
    /// Set for forks where the child should see the original project path.
    /// Non-worktree forks should keep this false so conversation paths are rewritten to the new cwd.
    pub skip_cwd_transform: bool,
    /// Stable display path for fork sessions.
    /// Persisted in the forked summary so the prompt-facing cwd survives session restore/reload.
    pub prompt_display_cwd: Option<String>,

    // ── Generic fork extensions (used by subagent + worktree forks) ──
    /// Override `session_kind` in the forked summary. Defaults to `"fork"`.
    /// Subagent resume sets `"subagent_resume"`.
    pub session_kind: Option<String>,
    /// How the fork's initial context was bootstrapped: `"new"` or `"forked"`.
    pub fork_context_source: Option<String>,
    /// Parent prompt/turn ID that triggered this fork.
    pub fork_parent_prompt_id: Option<String>,
    /// Whether to copy the plan state file. Defaults to `true`.
    pub copy_plan_state: bool,
    /// Whether to copy the plan mode state file. Defaults to `true`.
    pub copy_plan_mode_state: bool,
    /// Whether to copy the signals file. Defaults to `true`.
    pub copy_signals: bool,
    /// Whether to copy persisted usage. Defaults to `true`.
    /// Independent of `copy_signals` so a resume can keep billed history without parent telemetry.
    pub copy_usage: bool,
    /// Whether to copy `tool_state.json` (persisted tool state). Defaults to `true`.
    pub copy_tool_state: bool,
    /// Whether to copy `announcement_state.json`. Defaults to `true`.
    pub copy_announcement_state: bool,
    /// Whether to copy the `compaction/` segment archive (`segment_*.md` and `INDEX.md`, the verbose pre-compaction transcripts).
    /// Defaults to `false`: these can be large and most copy paths don't need them.
    /// Forks enable it so the child retains the parent's pre-compaction history.
    pub copy_compaction_segments: bool,
    /// When true, apply fork-safety filtering to copied chat history.
    /// Strip synthetic user messages (doom loop warnings, compaction metadata).
    /// Truncate at the last complete turn boundary.
    pub fork_filter: bool,
    /// Number of inherited parent conversation items.
    /// Stored in the child's summary so compaction can preserve the inherited prefix.
    pub inherited_prefix_len: Option<usize>,
    /// When true, strip `reasoning` (thinking/reasoning_content) from all assistant messages in the copied chat history.
    /// Set for forks so that the new session does not inherit the prior model's chain-of-thought.
    pub strip_reasoning: bool,
    /// The original workspace directory this worktree session was spawned from.
    /// Propagated to the forked session's `Summary::source_workspace_dir`.
    pub source_workspace_dir: Option<String>,
}

impl Default for CopySessionOptions {
    fn default() -> Self {
        Self {
            mint_session_identity: false,
            parent_session_id: None,
            new_model_id: None,
            target_prompt_index: None,
            skip_cwd_transform: false,
            prompt_display_cwd: None,
            session_kind: None,
            fork_context_source: None,
            fork_parent_prompt_id: None,
            copy_plan_state: true,
            copy_plan_mode_state: true,
            copy_signals: true,
            copy_usage: true,
            copy_tool_state: true,
            copy_announcement_state: true,
            copy_compaction_segments: false,
            fork_filter: false,
            inherited_prefix_len: None,
            strip_reasoning: false,
            source_workspace_dir: None,
        }
    }
}

/// Chunk `_meta.promptIndex` on an ACP `UserMessageChunk`, if present.
fn acp_user_chunk_prompt_index(update: &SessionUpdate) -> Option<usize> {
    let SessionUpdate::Acp(n) = update else {
        return None;
    };
    let acp::SessionUpdate::UserMessageChunk(chunk) = &n.update else {
        return None;
    };
    chunk
        .meta
        .as_ref()
        .and_then(|m| m.get("promptIndex"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
}

pub(crate) const HOST_TURN_META_KEY: &str = "hostTurn";

pub(crate) fn is_host_turn_chunk(chunk: &acp::ContentChunk) -> bool {
    chunk_meta_flag(chunk, HOST_TURN_META_KEY)
}

/// `ContentChunk._meta` flag on a persisted mid-turn interjection's user chunks. The text block keeps the
/// model-facing frame and carries the typed text in `displayText`; the pager and the chat rebuilders key on this.
pub const INTERJECTION_META_KEY: &str = "interjection";

pub fn is_interjection_chunk(chunk: &acp::ContentChunk) -> bool {
    chunk_meta_flag(chunk, INTERJECTION_META_KEY)
}

/// Boolean `ContentChunk._meta` flag; anything but a literal `true` reads as `false`.
pub fn chunk_meta_flag(chunk: &acp::ContentChunk, key: &str) -> bool {
    chunk
        .meta
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_bool())
        == Some(true)
}

fn is_host_turn_update(update: &SessionUpdate) -> bool {
    let SessionUpdate::Acp(n) = update else {
        return false;
    };
    let acp::SessionUpdate::UserMessageChunk(chunk) = &n.update else {
        return false;
    };
    is_host_turn_chunk(chunk)
}

fn is_acp_user_message_chunk(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::Acp(n) if matches!(n.update, acp::SessionUpdate::UserMessageChunk(_))
    )
}

/// Tracks user-message runs for turn counting (updates truncate / filter_rewind).
/// Progressive: every user run counts until the first `promptIndex` appears; after that only marked runs count (mid-turn phantoms omit the marker).
/// A change of `promptIndex` (including between unmarked and marked) opens a new run.
struct UserRunTurnTracker {
    seen_marker: bool,
    in_user: bool,
    /// `promptIndex` of the current user run (`None` means an unmarked phantom run).
    current_run_pi: Option<usize>,
}

impl UserRunTurnTracker {
    fn new() -> Self {
        Self {
            seen_marker: false,
            in_user: false,
            current_run_pi: None,
        }
    }

    /// Returns true if this user chunk opens a **counted** turn.
    fn on_user_chunk(&mut self, prompt_index: Option<usize>) -> bool {
        if prompt_index.is_some() {
            self.seen_marker = true;
        }
        let counts = if self.seen_marker {
            prompt_index.is_some()
        } else {
            true
        };
        let new_run = if !self.in_user {
            true
        } else if self.seen_marker || prompt_index.is_some() {
            prompt_index != self.current_run_pi
        } else {
            false
        };
        if new_run {
            self.current_run_pi = prompt_index;
            self.in_user = true;
            counts
        } else {
            self.in_user = true;
            false
        }
    }

    fn on_non_user(&mut self) {
        self.in_user = false;
        self.current_run_pi = None;
    }
}

/// How many items to keep for `target_prompt_index` (0-based, inclusive): the scan cuts at the opening chunk of the next counted turn.
/// Unmarked user runs count as turns only before the first `_meta.promptIndex`.
fn truncate_for_prompt_by<T>(
    items: &[T],
    target_prompt_index: usize,
    classify: impl Fn(&T) -> RewindStep,
) -> usize {
    let mut user_turn_count = 0;
    let mut tracker = UserRunTurnTracker::new();

    for (i, item) in items.iter().enumerate() {
        match classify(item) {
            RewindStep::UserChunk { prompt_index } => {
                if tracker.on_user_chunk(prompt_index) {
                    user_turn_count += 1;
                    if user_turn_count > target_prompt_index + 1 {
                        return i;
                    }
                }
            }
            RewindStep::Rewind { .. } | RewindStep::Other => tracker.on_non_user(),
        }
    }

    items.len()
}

#[derive(Debug)]
pub enum AppendUpdateError {
    NotCommitted(io::Error),
    Committed(io::Error),
}

#[derive(Debug)]
pub enum AppendChatError {
    NotCommitted(io::Error),
    Committed(io::Error),
}

#[derive(Debug)]
pub enum AppendCwdSwitchError {
    NotCommitted(io::Error),
    Committed {
        acknowledgement: distill_chat_state::StrictAppendAck,
        source: io::Error,
    },
}

impl AppendUpdateError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error,
        }
    }
}

impl std::fmt::Display for AppendUpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error.fmt(formatter),
        }
    }
}

impl AppendChatError {
    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error,
        }
    }
}

impl std::fmt::Display for AppendChatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error.fmt(formatter),
        }
    }
}

/// Session files a sync barrier flushes.
/// The persistence actor marks the files that took buffered writes since the last successful barrier.
/// Atomic-rename writes are durable at write time and never enter the set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionFileSet {
    pub updates: bool,
    pub chat: bool,
    pub summary: bool,
    pub plan: bool,
    pub rewind_points: bool,
}

impl SessionFileSet {
    pub(crate) const ALL: Self = Self {
        updates: true,
        chat: true,
        summary: true,
        plan: true,
        rewind_points: true,
    };

    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// Abstracts over different storage backends (JSONL, SQLite, etc.)
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    /// Initialize a new session or load existing one
    async fn init_session(&self, info: &Info, model_id: acp::ModelId) -> io::Result<Summary>;

    /// Set the session title unconditionally (manual `/rename`); last write wins.
    /// Also marks the title manual (`Summary::title_is_manual`) so clients restore the prompt-border title on resume.
    async fn update_session_title(&self, info: &Info, session_title: String) -> io::Result<()>;

    /// Set the session title only if the session has no title yet, used by automatic LLM title generation so it never overwrites a manual `/rename`.
    /// Never marks the title manual.
    /// Returns `true` if the title was written, `false` if an existing title was preserved.
    async fn set_generated_title_if_absent(
        &self,
        info: &Info,
        session_title: String,
    ) -> io::Result<bool>;

    /// Overwrite an existing auto title with a refreshed one (early-session title refresh at turns 3 and 6), but never a manual `/rename`.
    /// The manual check and write are atomic under the summary lock, so a concurrent manual rename always wins.
    /// Returns `true` if the title was written, `false` if a manual pin was preserved.
    async fn regenerate_generated_title(
        &self,
        info: &Info,
        session_title: String,
    ) -> io::Result<bool>;

    /// Persist the winning root identity atomically under the summary lock.
    async fn stamp_session_identity(
        &self,
        info: &Info,
        identity: crate::session::persistence::SessionIdentity,
    ) -> io::Result<crate::session::persistence::SessionIdentity>;

    /// Stamp `session_kind` only if the session has none yet, atomically under the summary lock.
    /// A kind already on disk (crash-recovered dir, concurrent writer) is preserved.
    async fn set_session_kind_if_absent(&self, info: &Info, kind: String) -> io::Result<()>;

    /// Clear a manual `/rename` pin (`/rename --auto`).
    /// Sets `title_is_manual = false` and, when a pin was present, blanks `generated_title` and `session_summary` so `display_title()` is empty.
    /// Idempotent when the title is not manual.
    async fn reset_title_to_auto(&self, info: &Info) -> io::Result<bool>;

    /// Replace or clear (`None`) the latest session recap preview in `summary.json`; last-writer-wins.
    /// Distinct from `last_turn_summary`.
    async fn set_last_recap(&self, info: &Info, recap: Option<String>) -> io::Result<()>;

    /// Replace or clear (`None`) the per-turn dashboard summary (`(text, prompt_id)`) in `summary.json`; last-writer-wins.
    async fn set_last_turn_summary(
        &self,
        info: &Info,
        summary: Option<(String, String)>,
    ) -> io::Result<()>;

    /// Append a session update (ACP update or Grok extension update) and increment counter
    async fn append_update(&self, info: &Info, update: &SessionUpdate) -> io::Result<()>;

    /// Append one update and report whether the replay record was committed before an error.
    async fn append_update_commit_aware(
        &self,
        info: &Info,
        update: &SessionUpdate,
    ) -> Result<(), AppendUpdateError> {
        self.append_update(info, update)
            .await
            .map_err(AppendUpdateError::NotCommitted)
    }

    /// Append one update durably, preserving whether the replay record committed before failure.
    async fn append_update_durable_commit_aware(
        &self,
        _info: &Info,
        _update: &SessionUpdate,
    ) -> Result<(), AppendUpdateError> {
        Err(AppendUpdateError::NotCommitted(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable session update append is unsupported",
        )))
    }

    /// Append a chat message and increment counter.
    async fn append_chat_message(&self, info: &Info, message: &ConversationItem) -> io::Result<()>;

    /// Append one chat message and report whether the JSONL record was committed before an error.
    /// Bookkeeping (summary counters) can fail after the append has already reached the page cache.
    async fn append_chat_message_commit_aware(
        &self,
        info: &Info,
        message: &ConversationItem,
    ) -> Result<(), AppendChatError> {
        self.append_chat_message(info, message)
            .await
            .map_err(AppendChatError::NotCommitted)
    }

    /// Append one working-directory switch generation exactly once.
    async fn append_cwd_switch_commit_aware(
        &self,
        _info: &Info,
        _message: &ConversationItem,
    ) -> Result<distill_chat_state::StrictAppendAck, AppendCwdSwitchError> {
        Err(AppendCwdSwitchError::NotCommitted(io::Error::new(
            io::ErrorKind::Unsupported,
            "working-directory switch append is unsupported",
        )))
    }

    /// Update the current model in summary (delegates to `update_current_model_and_agent` with `agent_name = None`).
    async fn update_current_model(&self, info: &Info, model_id: &acp::ModelId) -> io::Result<()> {
        self.update_current_model_and_agent(info, model_id, None, None)
            .await
    }

    /// Update the current model and agent name in summary.
    /// `agent_name` is the resolved agent definition name, persisted so session resume doesn't depend on the mutable model catalog.
    /// `None` leaves the existing `agent_name` unchanged (used by legacy callers that only update the model ID).
    async fn update_current_model_and_agent(
        &self,
        info: &Info,
        model_id: &acp::ModelId,
        agent_name: Option<&str>,
        reasoning_effort: Option<Option<ReasoningEffort>>,
    ) -> io::Result<()>;

    /// Update the collection ID for telemetry tracing
    async fn update_collection_id(&self, info: &Info, collection_id: &str) -> io::Result<()>;

    /// Update the persisted HEAD commit and branch in summary
    async fn update_git_head(
        &self,
        info: &Info,
        commit: Option<String>,
        branch: Option<String>,
    ) -> io::Result<()>;

    /// Update the monotonic telemetry trace turn counter ("next turn" value).
    async fn update_next_trace_turn(
        &self,
        info: &Info,
        next_trace_turn: u64,
        request_id: Option<&str>,
    ) -> io::Result<()>;

    async fn update_wake_start(
        &self,
        info: &Info,
        start: crate::session::persistence::WakeStart,
        abort: tokio_util::sync::CancellationToken,
    ) -> io::Result<()>;

    async fn restore_wake_summary(
        &self,
        info: &Info,
        prior: crate::session::persistence::WakeSummaryState,
    ) -> io::Result<()>;

    async fn write_plan_state(&self, info: &Info, state: &TodoState) -> io::Result<()>;

    async fn write_plan_mode_state(
        &self,
        info: &Info,
        state: &crate::session::plan_mode::PlanModeSnapshot,
    ) -> io::Result<()>;

    async fn write_signals(&self, info: &Info, signals: &SessionSignals) -> io::Result<()>;

    async fn read_usage(
        &self,
        info: &Info,
    ) -> io::Result<Option<crate::session::usage_file::SessionUsageFile>>;

    async fn write_usage(
        &self,
        info: &Info,
        usage: &crate::session::usage_file::SessionUsageFile,
    ) -> io::Result<()>;

    async fn write_announcement_state(
        &self,
        info: &Info,
        state: &crate::session::announcement_state::AnnouncementState,
    ) -> io::Result<()>;

    async fn write_goal_mode_state(
        &self,
        info: &Info,
        state: &crate::session::goal_tracker::GoalOrchestration,
    ) -> io::Result<()>;

    async fn delete_goal_mode_state(&self, info: &Info) -> io::Result<()>;

    async fn write_workflow_run_state(
        &self,
        info: &Info,
        manifest: &crate::session::workflow::store::WorkflowRunManifest,
    ) -> io::Result<()>;

    async fn delete_workflow_run_state(&self, info: &Info, run_id: &str) -> io::Result<()>;

    async fn load_session(&self, info: &Info) -> io::Result<PersistedData>;

    /// Load session data WITHOUT updates (for memory efficiency when updates will be streamed).
    /// Implementations also do NOT read rewind points here.
    /// Those are deferred and lazily loaded on demand from the path returned by [`rewind_points_file_path`](StorageAdapter::rewind_points_file_path).
    async fn load_session_without_updates(&self, info: &Info) -> io::Result<PersistedDataLight>;

    async fn load_summary(&self, info: &Info) -> io::Result<Summary>;

    /// When `cwd` is `None`, returns summaries for all sessions.
    async fn list_sessions(&self, cwd: Option<&str>) -> io::Result<Vec<Summary>>;

    /// Permanently delete a session's stored data (all files for the session).
    /// Implementations must treat a missing session as success (idempotent delete).
    async fn delete_session(&self, info: &Info) -> io::Result<()>;

    async fn append_rewind_point(&self, info: &Info, point: &RewindPoint) -> io::Result<()>;

    async fn load_rewind_points(&self, info: &Info) -> io::Result<Vec<RewindPoint>>;

    /// Sync the selected session files, plus the session directory entry once, to stable media.
    /// Backs the `FlushAndAck` barrier (dirty files only) and the pre-`CopyFile` flush ([`SessionFileSet::ALL`]).
    /// An error means the barrier must not ack.
    async fn sync_session_files_selected(
        &self,
        info: &Info,
        files: SessionFileSet,
    ) -> io::Result<()>;

    /// Truncate rewind points from a specific prompt index (inclusive)
    /// Used when rewinding to remove future history
    async fn truncate_rewind_points_from(&self, info: &Info, from_index: usize) -> io::Result<()>;

    /// Merge rewind points at indices `>= target_index` into the point at `target_index - 1` and drop the folded points.
    /// Runs as a read-modify-write on disk (used after a ConversationOnly rewind).
    /// Reading the current on-disk set makes this authoritative.
    async fn merge_rewind_points_from(&self, info: &Info, target_index: usize) -> io::Result<()>;

    /// Replace the entire chat history (used for compaction and rewind)
    async fn replace_chat_history(
        &self,
        info: &Info,
        messages: &[ConversationItem],
    ) -> io::Result<()>;

    /// Copy the on-disk chat history before a destructive image-strip rewrite (first backup wins), mirroring the `*.corrupt` quarantine.
    /// Required, not defaulted: a new adapter must choose explicitly how its data stays recoverable.
    async fn backup_chat_history_before_strip(&self, info: &Info) -> io::Result<()>;

    /// Copy session data from source to target, transforming session IDs
    async fn copy_session_data(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: CopySessionOptions,
    ) -> io::Result<CopySessionResult>;

    /// Load only user prompts from a session's updates file.
    /// Returns user prompts in chronological order.
    async fn load_prompts_only(&self, info: &Info) -> io::Result<Vec<String>>;
    /// Load assistant text content from a session's updates file.
    /// Returns assistant responses in chronological order, extracted from ContentChunk text.
    async fn load_assistant_text(&self, info: &Info) -> io::Result<Vec<String>>;

    /// Tool name: from `ToolCall.title` (display name; acp::ToolCall has no .name field).
    /// File paths: from `ToolCall.locations[].path` (ACP stores locations, not parsed arguments).
    /// Errors: skipped (no is_error field on acp::SessionUpdate::ToolCallUpdate).
    async fn load_tool_metadata(&self, info: &Info) -> io::Result<Vec<String>>;

    /// Get the path to the updates file for streaming reads.
    /// Returns None if the storage backend doesn't support streaming.
    fn updates_file_path(&self, info: &Info) -> Option<std::path::PathBuf>;

    /// Path to the rewind-points file for lazy/deferred loading, or None if the backend doesn't persist them to a streamable file.
    /// The adapter owns the on-disk layout, so callers must use this rather than recomputing the path.
    /// The path differs for non-default storage modes, e.g. subagent/fork sessions.
    fn rewind_points_file_path(&self, info: &Info) -> Option<std::path::PathBuf>;

    /// Append a feedback entry (user feedback) to feedback.jsonl
    async fn append_feedback(
        &self,
        info: &Info,
        entry: &crate::session::persistence::LocalFeedbackEntry,
    ) -> io::Result<()>;

    /// Append a /btw side question entry to btw_history.jsonl
    async fn append_btw(
        &self,
        info: &Info,
        entry: &crate::session::persistence::BtwEntry,
    ) -> io::Result<()>;

    /// Write a compaction checkpoint file to `compaction_checkpoints/{checkpoint_id}.json`.
    async fn write_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint: &crate::extensions::notification::CompactionCheckpointFile,
    ) -> io::Result<()>;

    /// Write a compaction request artifact to `compaction_requests/{request_id}.json`.
    /// Captures the exact request sent to the compaction model and the response (or final error) it produced.
    /// Used for offline prompt iteration.
    async fn write_compaction_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::CompactionRequestFile,
    ) -> io::Result<()>;

    /// Write a recap request artifact to `recap_requests/{request_id}.json`.
    /// Captures the exact request sent for `/recap` or auto recap and the response (or error).
    /// Used for offline recap prompt and garble analysis.
    async fn write_recap_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::RecapRequestFile,
    ) -> io::Result<()>;

    /// Render and write `compaction/segment_NNN.md` (storage assigns the resume-safe index) and append its `INDEX.md` row.
    async fn write_compaction_segment(
        &self,
        info: &Info,
        segment: &crate::extensions::notification::CompactionSegmentFile,
    ) -> io::Result<()>;

    /// Read a compaction checkpoint file by its relative path within the session directory.
    async fn read_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint_file: &str,
    ) -> io::Result<crate::extensions::notification::CompactionCheckpointFile>;
}

/// Backup-gated strip rewrite: the destructive rewrite runs only when the backup landed.
/// So recoverability can never be silently forfeited (full disk, read-only volume).
/// Factored out of the persistence actor so the gate ordering is testable against a real adapter.
pub(crate) async fn strip_rewrite_gated(
    storage: &dyn StorageAdapter,
    info: &Info,
    messages: &[ConversationItem],
) -> io::Result<()> {
    storage.backup_chat_history_before_strip(info).await?;
    storage.replace_chat_history(info, messages).await
}

pub use jsonl::JsonlStorageAdapter;
#[cfg(any(test, feature = "test-support"))]
pub use replay::load_updates_for_replay_at;
pub use replay::{
    PreparedReplay, ReplayEmission, ReplayLookupFallback, ReplayPathHint, ReplayedUpdate,
    load_updates_for_replay, prepare_replay_lines, replay_would_emit, stream_replay_updates_at,
    stream_replay_updates_at_hinted,
};
pub(crate) use replay::{ReplayToolCollapser, UnfinishedSubagent, filter_delta_replay_lines};

/// Extracts `method` and raw `params` from an updates.jsonl envelope without parsing the notification payload.
#[derive(serde::Deserialize)]
pub(crate) struct RawLinePeek<'a> {
    #[serde(default)]
    pub method: Option<&'a str>,
    #[serde(borrow, default)]
    pub params: Option<&'a serde_json::value::RawValue>,
}

/// Peeks at `update.sessionUpdate` tag and `_meta` without full deserialization.
#[derive(serde::Deserialize)]
pub(crate) struct RawParamsPeek<'a> {
    #[serde(borrow, default)]
    pub update: Option<RawUpdatePeek<'a>>,
    #[serde(borrow, default, rename = "_meta")]
    pub meta: Option<&'a serde_json::value::RawValue>,
}

#[derive(serde::Deserialize)]
pub(crate) struct RawUpdatePeek<'a> {
    #[serde(rename = "sessionUpdate")]
    pub session_update: &'a str,
    #[serde(default)]
    pub status: Option<&'a str>,
    #[serde(default)]
    pub target_prompt_index: Option<usize>,
    /// Chunk `_meta.promptIndex` when present (owned; not borrowed).
    #[serde(default, rename = "_meta")]
    pub meta: Option<RawChunkMetaPeek>,
}

#[derive(serde::Deserialize)]
pub(crate) struct RawChunkMetaPeek {
    #[serde(default, rename = "promptIndex")]
    pub prompt_index: Option<u64>,
    #[serde(default, rename = "hostTurn")]
    pub host_turn: Option<bool>,
}

/// Role of one item in the rewind timeline, as seen by [`filter_rewind_by`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RewindStep {
    /// Rewind marker: truncate survivors back to `target`'s prompt boundary.
    Rewind { target: usize },
    /// User-message chunk opening (or continuing) a prompt run.
    UserChunk { prompt_index: Option<usize> },
    /// Anything else: kept, but ends the current user run.
    Other,
}

/// Shared rewind dead-branch filter. `classify` maps each item to its [`RewindStep`].
/// The driver tracks prompt boundaries and, on a marker, truncates survivors back to the target prompt.
/// [`filter_rewind_lines`] and [`filter_rewind_updates`] wrap this over raw JSONL and typed updates so the two paths share one algorithm.
fn filter_rewind_by<T>(items: Vec<T>, classify: impl Fn(&T) -> RewindStep) -> Vec<T> {
    let mut result: Vec<T> = Vec::with_capacity(items.len());
    let mut prompt_starts: Vec<usize> = Vec::new();
    let mut tracker = UserRunTurnTracker::new();

    for item in items {
        match classify(&item) {
            RewindStep::Rewind { target } => {
                // Out-of-range target keeps every survivor: fold to `result.len()`.
                let trunc = prompt_starts.get(target).copied().unwrap_or(result.len());
                result.truncate(trunc);
                prompt_starts.truncate(target);
                tracker.on_non_user();
                continue;
            }
            RewindStep::UserChunk { prompt_index } => {
                if tracker.on_user_chunk(prompt_index) {
                    prompt_starts.push(result.len());
                }
            }
            RewindStep::Other => tracker.on_non_user(),
        }
        result.push(item);
    }
    result
}

/// Classify a raw JSONL line by peeking at its tag and `_meta` without fully deserializing the payload.
fn rewind_step_for_line(line: &str) -> RewindStep {
    let (raw_params, is_xai) = if let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(line) {
        let raw = env.params.map(|p| p.get()).unwrap_or(line);
        (raw, env.method == Some(XAI_SESSION_UPDATE_METHOD))
    } else {
        (line, false)
    };

    let Some(u) = serde_json::from_str::<RawParamsPeek<'_>>(raw_params)
        .ok()
        .and_then(|p| p.update)
    else {
        return RewindStep::Other;
    };

    if is_xai
        && u.session_update == *REWIND_MARKER
        && let Some(target) = u.target_prompt_index
    {
        return RewindStep::Rewind { target };
    }

    let is_host_turn = u.meta.as_ref().and_then(|m| m.host_turn).unwrap_or(false);
    if !is_xai && !is_host_turn && u.session_update == *USER_MESSAGE_CHUNK {
        let prompt_index = u
            .meta
            .as_ref()
            .and_then(|m| m.prompt_index.map(|v| v as usize));
        return RewindStep::UserChunk { prompt_index };
    }

    RewindStep::Other
}

fn rewind_step_for_update(update: &SessionUpdate) -> RewindStep {
    if let SessionUpdate::Xai(n) = update
        && let crate::extensions::notification::SessionUpdate::RewindMarker {
            target_prompt_index,
            ..
        } = &n.update
    {
        return RewindStep::Rewind {
            target: *target_prompt_index,
        };
    }
    if is_acp_user_message_chunk(update) && !is_host_turn_update(update) {
        return RewindStep::UserChunk {
            prompt_index: acp_user_chunk_prompt_index(update),
        };
    }
    RewindStep::Other
}

/// Canonical raw-line rewind filter used by the initial and delta replay paths.
/// Skips parsing entirely when no rewind markers are present.
pub(crate) fn filter_rewind_lines(lines: Vec<&str>) -> Vec<&str> {
    if !lines.iter().any(|l| l.contains(&*REWIND_MARKER)) {
        return lines;
    }
    filter_rewind_by(lines, |line| rewind_step_for_line(line))
}

/// Typed equivalent of [`filter_rewind_lines`] over the same [`filter_rewind_by`] driver.
pub fn filter_rewind_updates(updates: Vec<SessionUpdate>) -> Vec<SessionUpdate> {
    let has_rewinds = updates.iter().any(|u| {
        matches!(
            u,
            SessionUpdate::Xai(n) if matches!(
                n.update,
                crate::extensions::notification::SessionUpdate::RewindMarker { .. }
            )
        )
    });
    if !has_rewinds {
        return updates;
    }
    filter_rewind_by(updates, rewind_step_for_update)
}

/// Strip `<fork-context>` and `<resume-context>` XML wrappers from user message chunks so replayed/exported prompts show clean text.
/// The tags are injected by the subagent fork/resume logic in `subagent.rs`.
pub fn strip_context_wrappers(update: acp::SessionUpdate) -> acp::SessionUpdate {
    let acp::SessionUpdate::UserMessageChunk(mut chunk) = update else {
        return update;
    };
    if let acp::ContentBlock::Text(ref mut t) = chunk.content {
        for tag in &["fork-context", "resume-context"] {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if let Some(start) = t.text.find(&open)
                && let Some(rel_end) = t
                    .text
                    .get(start + open.len()..)
                    .and_then(|s| s.find(&close))
            {
                let end = start + open.len() + rel_end;
                let remove_end = end + close.len();
                t.text = format!(
                    "{}{}",
                    t.text.get(..start).unwrap_or(""),
                    t.text.get(remove_end..).unwrap_or("").trim_start()
                );
            }
        }
    }
    acp::SessionUpdate::UserMessageChunk(chunk)
}

/// The session dir's `updates.jsonl` path if it exists, else `None`.
/// Sole owner of the "does this dir have a replayable updates file" gate.
pub(crate) fn replay_updates_path_in_dir(
    session_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let updates_path = session_dir.join(UPDATES_FILE);
    updates_path.exists().then_some(updates_path)
}

// ============================================================================
// Selective prompt-extraction parser
// ============================================================================

/// Each event represents the minimal information extracted from one `updates.jsonl` line without deserializing the full typed notification.
#[derive(Debug, PartialEq)]
pub enum PromptExtractEvent {
    /// A text chunk from a `UserMessageChunk` ACP update.
    /// Multiple consecutive `UserTextChunk` events belong to the same user message and should be concatenated by the caller.
    /// `prompt_index` is the chunk `_meta.promptIndex` when the turn pipeline stamped one.
    UserTextChunk {
        text: String,
        prompt_index: Option<usize>,
    },

    /// A `RewindMarker` Grok update: truncate accumulated prompts to this index.
    ///
    /// Any in-progress user message should be flushed before truncating.
    RewindTo(usize),

    /// Any other update type: the current user message (if any) has ended.
    NotUserMessage,
}

impl PromptExtractEvent {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::UserTextChunk {
            text: text.into(),
            prompt_index: None,
        }
    }

    pub fn user_text_pi(text: impl Into<String>, prompt_index: usize) -> Self {
        Self::UserTextChunk {
            text: text.into(),
            prompt_index: Some(prompt_index),
        }
    }
}

/// Iterator that streams [`PromptExtractEvent`]s from a `updates.jsonl` file.
/// Unlike [`UpdatesIterator`], this never builds a full `acp::SessionNotification` or `SessionNotification`.
/// Instead it uses zero-copy `serde_json` deserialization with `&RawValue` to peek at the discriminant field.
pub struct PromptExtractIterator {
    reader: std::io::BufReader<std::fs::File>,
    line_buffer: String,
}

impl PromptExtractIterator {
    /// Returns `None` if the file does not exist.
    pub fn open(path: &std::path::Path) -> std::io::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        Ok(Some(Self {
            reader: std::io::BufReader::new(file),
            line_buffer: String::new(),
        }))
    }
}

impl Iterator for PromptExtractIterator {
    type Item = PromptExtractEvent;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.line_buffer.clear();
            match std::io::BufRead::read_line(&mut self.reader, &mut self.line_buffer) {
                Ok(0) => return None, // EOF
                Err(_) => return Some(PromptExtractEvent::NotUserMessage),
                Ok(_) => {}
            }

            let line = self.line_buffer.trim();
            if line.is_empty() {
                continue;
            }

            return Some(parse_prompt_extract_event(line));
        }
    }
}

/// Consecutive `UserTextChunk` events are concatenated into one prompt until a non-user event or a `promptIndex` change opens a new run.
/// Progressive counting (same as [`UserRunTurnTracker`]): every user run counts until the first `_meta.promptIndex`.
/// After that only marked runs count (mid-turn phantoms are dropped from the list).
pub fn collect_prompts_from_events(iter: impl Iterator<Item = PromptExtractEvent>) -> Vec<String> {
    let mut prompts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_user = false;
    let mut current_run_pi: Option<usize> = None;
    let mut current_counts = false;
    let mut seen_marker = false;

    fn flush(
        prompts: &mut Vec<String>,
        current: &mut String,
        in_user: &mut bool,
        current_run_pi: &mut Option<usize>,
        current_counts: &mut bool,
    ) {
        if *in_user {
            if *current_counts {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    prompts.push(trimmed);
                }
            }
            current.clear();
            *in_user = false;
            *current_run_pi = None;
            *current_counts = false;
        }
    }

    for event in iter {
        match event {
            PromptExtractEvent::UserTextChunk { text, prompt_index } => {
                if prompt_index.is_some() {
                    seen_marker = true;
                }
                let counts = if seen_marker {
                    prompt_index.is_some()
                } else {
                    true
                };
                let new_run = if !in_user {
                    true
                } else if seen_marker || prompt_index.is_some() {
                    prompt_index != current_run_pi
                } else {
                    false
                };
                if new_run {
                    flush(
                        &mut prompts,
                        &mut current,
                        &mut in_user,
                        &mut current_run_pi,
                        &mut current_counts,
                    );
                    in_user = true;
                    current_run_pi = prompt_index;
                    current_counts = counts;
                    current.push_str(&text);
                } else {
                    current.push_str(&text);
                    if current_run_pi.is_none() && prompt_index.is_some() {
                        current_run_pi = prompt_index;
                        current_counts = true;
                    }
                }
            }
            PromptExtractEvent::RewindTo(target_index) => {
                // Flush any in-progress user message before truncating.
                // Rewinding TO prompt N keeps prompts[0..N].
                flush(
                    &mut prompts,
                    &mut current,
                    &mut in_user,
                    &mut current_run_pi,
                    &mut current_counts,
                );
                prompts.truncate(target_index);
            }
            PromptExtractEvent::NotUserMessage => {
                flush(
                    &mut prompts,
                    &mut current,
                    &mut in_user,
                    &mut current_run_pi,
                    &mut current_counts,
                );
            }
        }
    }

    flush(
        &mut prompts,
        &mut current,
        &mut in_user,
        &mut current_run_pi,
        &mut current_counts,
    );

    prompts
}
/// Extracts `ContentChunk.text` from `AgentMessageChunk` updates.
/// Rewound-away branches may still contribute to FTS index.
pub fn collect_assistant_text(
    iter: impl Iterator<Item = io::Result<SessionUpdate>>,
) -> Vec<String> {
    const MAX_CHARS: usize = 100_000;
    let mut texts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars_emitted = 0usize;

    for res in iter {
        let update = match res {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "skipping malformed update in assistant text collector");
                continue;
            }
        };
        match update {
            SessionUpdate::Acp(notification) => {
                match notification.update {
                    acp::SessionUpdate::AgentMessageChunk(chunk) => {
                        if let acp::ContentBlock::Text(text_content) = chunk.content
                            && !text_content.text.is_empty()
                        {
                            // Reserve space for separator before computing budget to avoid overshoot
                            let sep_cost = usize::from(!current.is_empty());
                            let budget = MAX_CHARS
                                .saturating_sub(chars_emitted)
                                .saturating_sub(sep_cost);
                            if budget == 0 {
                                continue;
                            }
                            let text = if text_content.text.len() > budget {
                                // Truncate on a valid UTF-8 char boundary
                                let mut end = budget;
                                while end > 0 && !text_content.text.is_char_boundary(end) {
                                    end -= 1;
                                }
                                text_content.text.get(..end).unwrap_or("")
                            } else {
                                &text_content.text
                            };
                            if !current.is_empty() {
                                current.push(' ');
                                chars_emitted += 1;
                            }
                            current.push_str(text);
                            chars_emitted += text.len();
                        }
                    }
                    _ => {
                        // End of assistant turn
                        if !current.is_empty() {
                            let t = current.trim().to_string();
                            if !t.is_empty() {
                                texts.push(t);
                            }
                            current.clear();
                        }
                    }
                }
            }
            SessionUpdate::Xai(_) => {
                if !current.is_empty() {
                    let t = current.trim().to_string();
                    if !t.is_empty() {
                        texts.push(t);
                    }
                    current.clear();
                }
            }
        }
    }
    if !current.is_empty() {
        let t = current.trim().to_string();
        if !t.is_empty() {
            texts.push(t);
        }
    }
    texts
}

/// Tool name: from `ToolCall.title` (display name; acp::ToolCall has no .name).
/// Errors: skipped (no is_error on acp::ToolCallUpdate).
/// Each extraction capped at 100k chars before final join.
pub fn collect_tool_metadata(iter: impl Iterator<Item = io::Result<SessionUpdate>>) -> Vec<String> {
    let mut meta: Vec<String> = Vec::new();
    let mut tool_call_count = 0usize;
    let mut chars_emitted = 0usize;

    const MAX_TOOL_CALLS: usize = 200;
    const MAX_CHARS: usize = 100_000;

    for res in iter {
        if tool_call_count >= MAX_TOOL_CALLS {
            break;
        }
        let update = match res {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "skipping malformed update in tool metadata collector");
                continue;
            }
        };
        match update {
            SessionUpdate::Acp(notification) => {
                match notification.update {
                    acp::SessionUpdate::ToolCall(tc) => {
                        tool_call_count += 1;

                        // Tool name from .title (acp::ToolCall has no .name field)
                        if !tc.title.is_empty() {
                            let budget = MAX_CHARS.saturating_sub(chars_emitted);
                            if budget == 0 {
                                continue;
                            }
                            let truncated =
                                tc.title.get(..tc.title.len().min(budget)).unwrap_or("");
                            chars_emitted += truncated.len();
                            meta.push(truncated.to_string());
                        }

                        // File paths from locations[].path
                        for loc in &tc.locations {
                            if let Some(path_str) = loc.path.to_str()
                                && !path_str.is_empty()
                            {
                                let budget = MAX_CHARS.saturating_sub(chars_emitted);
                                if budget == 0 {
                                    continue;
                                }
                                let truncated =
                                    path_str.get(..path_str.len().min(budget)).unwrap_or("");
                                meta.push(truncated.to_string());
                                chars_emitted += truncated.len();
                            }
                        }
                    }
                    acp::SessionUpdate::ToolCallUpdate(_) => {
                        // Tool results come as ToolCallUpdate; no is_error field available
                    }
                    _ => {}
                }
            }
            SessionUpdate::Xai(_) => {}
        }
    }
    meta
}

// ---------------------------------------------------------------------------
// Selective serde structs: only the fields we care about
// ---------------------------------------------------------------------------

/// Peek inside ACP or Grok `params` to read the `update.sessionUpdate` tag and any fields relevant to `user_message_chunk` or `rewind_marker`.
///
/// Works for both method types because both use the same `update.sessionUpdate` discriminant key in the params JSON.
#[derive(serde::Deserialize)]
struct ParamsPeek<'a> {
    #[serde(borrow)]
    update: UpdatePeek<'a>,
}

#[derive(serde::Deserialize)]
struct UpdatePeek<'a> {
    #[serde(rename = "sessionUpdate")]
    session_update: &'a str,
    /// Present only for `user_message_chunk`.
    #[serde(borrow, default)]
    content: Option<ContentPeek<'a>>,
    /// Chunk `_meta` on ACP updates (carries `promptIndex` for real turns).
    #[serde(default, rename = "_meta")]
    meta: Option<RawChunkMetaPeek>,
    /// Present only for `rewind_marker`.
    target_prompt_index: Option<usize>,
}

/// Selective peek at a `user_message_chunk` content object.
/// Shared with the search collectors in [`search`].
/// The peeked fields and their escape-tolerance therefore cannot drift between the prompt-extraction and indexing paths.
#[derive(serde::Deserialize)]
pub(crate) struct ContentPeek<'a> {
    #[serde(rename = "type", default)]
    pub content_type: Option<&'a str>,
    // `Cow`, not `&str`: serde cannot borrow from JSON strings containing escapes, and the resulting parse error would drop the whole prompt
    #[serde(borrow, default)]
    pub text: Option<std::borrow::Cow<'a, str>>,
    #[serde(rename = "_meta", default)]
    pub meta: Option<ContentMetaPeek<'a>>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ContentMetaPeek<'a> {
    #[serde(borrow, default)]
    pub bash_command: Option<std::borrow::Cow<'a, str>>,
}

/// Parse one `updates.jsonl` line into a [`PromptExtractEvent`].
/// Always returns an event: `NotUserMessage` for every line that is not a user-message chunk or rewind marker (including unparseable ones).
/// Fast path: only those two kinds can produce a non-`NotUserMessage` event, and their discriminant appears verbatim.
pub(crate) fn parse_prompt_extract_event(line: &str) -> PromptExtractEvent {
    if !line.contains(&*USER_MESSAGE_CHUNK) && !line.contains(&*REWIND_MARKER) {
        return PromptExtractEvent::NotUserMessage;
    }

    // Step 1: try to extract the envelope (method and raw params)
    let (raw_params, is_xai) = if let Ok(env) = serde_json::from_str::<RawLinePeek<'_>>(line) {
        let raw = env.params.map(|p| p.get()).unwrap_or(line);
        let xai = env.method == Some(XAI_SESSION_UPDATE_METHOD);
        (raw, xai)
    } else {
        // Not a valid envelope, so try legacy format: the line IS the params
        (line, false)
    };

    // Step 2: parse the discriminant and relevant payload fields in one pass.
    let Ok(peek) = serde_json::from_str::<ParamsPeek<'_>>(raw_params) else {
        // Cannot determine update type, so treat conservatively
        return PromptExtractEvent::NotUserMessage;
    };

    let tag = peek.update.session_update;

    if !is_xai && tag == *USER_MESSAGE_CHUNK {
        if let Some(content) = peek.update.content
            && content.content_type == Some("text")
            && let Some(text) = content.text
        {
            if content
                .meta
                .as_ref()
                .is_some_and(|m| m.bash_command.is_some())
            {
                return PromptExtractEvent::NotUserMessage;
            }
            if peek
                .update
                .meta
                .as_ref()
                .is_some_and(|m| m.host_turn == Some(true))
            {
                return PromptExtractEvent::NotUserMessage;
            }
            let prompt_index = peek
                .update
                .meta
                .as_ref()
                .and_then(|m| m.prompt_index.map(|v| v as usize));
            return PromptExtractEvent::UserTextChunk {
                text: text.into_owned(),
                prompt_index,
            };
        }
        // user_message_chunk with non-text content (e.g., image) still ends any in-progress user message
        return PromptExtractEvent::NotUserMessage;
    }

    if is_xai && tag == *REWIND_MARKER {
        if let Some(idx) = peek.update.target_prompt_index {
            return PromptExtractEvent::RewindTo(idx);
        }
        // Malformed rewind_marker: treat conservatively (flush, no truncate).
        return PromptExtractEvent::NotUserMessage;
    }

    PromptExtractEvent::NotUserMessage
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    mod rebuild_interjections {
        use super::*;
        use crate::sampling::{ContentPart, ConversationItem, SyntheticReason};
        use std::sync::Arc;

        fn text_chunk(text: &str) -> acp::ContentChunk {
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)))
        }

        fn user(text: &str) -> acp::SessionUpdate {
            acp::SessionUpdate::UserMessageChunk(text_chunk(text))
        }

        fn agent(text: &str) -> acp::SessionUpdate {
            acp::SessionUpdate::AgentMessageChunk(text_chunk(text))
        }

        /// A persisted interjection chunk as the shell writes it: framed text plus the `interjection` flag.
        fn interjection(typed: &str) -> acp::SessionUpdate {
            let mut meta = serde_json::Map::new();
            meta.insert(INTERJECTION_META_KEY.into(), serde_json::json!(true));
            acp::SessionUpdate::UserMessageChunk(
                text_chunk(&distill_interjection_core::format_interjection(
                    typed.to_string(),
                ))
                .meta(Some(meta)),
            )
        }

        fn tool_call(id: &str) -> acp::SessionUpdate {
            acp::SessionUpdate::ToolCall(acp::ToolCall::new(acp::ToolCallId::new(id), "run"))
        }

        fn tool_done(id: &str) -> acp::SessionUpdate {
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(id),
                acp::ToolCallUpdateFields::new().status(Some(acp::ToolCallStatus::Completed)),
            ))
        }

        fn rebuild(updates: Vec<acp::SessionUpdate>) -> Vec<ConversationItem> {
            let sid = acp::SessionId::new(Arc::from("s"));
            let dir = tempfile::tempdir().unwrap();
            let envelopes: Vec<SessionUpdateEnvelope> = updates
                .into_iter()
                .map(|update| {
                    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(sid.clone(), update)))
                })
                .map(|u| SessionUpdateEnvelope::from_update(&u).unwrap())
                .collect();
            write_jsonl_atomic(&dir.path().join(UPDATES_FILE), &envelopes).unwrap();
            chat_rebuild::rebuild_chat_history(dir.path()).unwrap();
            std::fs::read_to_string(dir.path().join(CHAT_HISTORY_FILE))
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }

        /// `(text, is_interjection)` of every user item, in order.
        fn users(items: &[ConversationItem]) -> Vec<(String, bool)> {
            items
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::User(u) => Some((
                        u.content
                            .iter()
                            .filter_map(|p| match p {
                                ContentPart::Text { text } => Some(text.as_ref()),
                                _ => None,
                            })
                            .collect(),
                        u.synthetic_reason == SyntheticReason::Interjection,
                    )),
                    _ => None,
                })
                .collect()
        }

        fn framed(typed: &str) -> String {
            distill_interjection_core::format_interjection(typed.to_string())
        }

        #[test]
        fn interjection_keeps_persisted_frame_and_is_tagged() {
            let items = rebuild(vec![
                user("start the build"),
                agent("building"),
                interjection("ok run the stop for me"),
                agent("stopping"),
            ]);
            assert_eq!(
                users(&items),
                [
                    ("start the build".to_string(), false),
                    (framed("ok run the stop for me"), true),
                ]
            );
        }

        /// Tool-only first response: no agent text closes the prompt run before the interjection lands.
        #[test]
        fn interjection_adjacent_to_prompt_run_is_a_separate_item() {
            let items = rebuild(vec![
                user("start the build"),
                tool_call("c1"),
                tool_done("c1"),
                interjection("ok run the stop for me"),
                agent("stopping"),
            ]);
            assert_eq!(
                users(&items),
                [
                    ("start the build".to_string(), false),
                    (framed("ok run the stop for me"), true),
                ]
            );
        }

        /// A batch drain pushes one item per interjection; a following prompt echo must not join the last one.
        #[test]
        fn consecutive_interjections_stay_distinct_and_next_prompt_is_plain() {
            let items = rebuild(vec![
                user("start"),
                agent("working"),
                interjection("first"),
                interjection("second"),
                user("next prompt"),
                agent("done"),
            ]);
            assert_eq!(
                users(&items),
                [
                    ("start".to_string(), false),
                    (framed("first"), true),
                    (framed("second"), true),
                    ("next prompt".to_string(), false),
                ]
            );
        }
    }

    #[test]
    fn chunk_meta_flag_requires_literal_true() {
        let chunk = |meta: Option<serde_json::Value>| {
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new("x")))
                .meta(meta.map(|m| m.as_object().cloned().unwrap()))
        };
        assert!(chunk_meta_flag(
            &chunk(Some(serde_json::json!({"k": true}))),
            "k"
        ));
        assert!(!chunk_meta_flag(
            &chunk(Some(serde_json::json!({"k": false}))),
            "k"
        ));
        assert!(!chunk_meta_flag(
            &chunk(Some(serde_json::json!({"k": "true"}))),
            "k"
        ));
        assert!(!chunk_meta_flag(
            &chunk(Some(serde_json::json!({"k": 1}))),
            "k"
        ));
        assert!(!chunk_meta_flag(
            &chunk(Some(serde_json::json!({"other": true}))),
            "k"
        ));
        assert!(!chunk_meta_flag(&chunk(None), "k"));
    }

    #[test]
    fn atomic_write_runs_file_sync_barrier_before_rename_replaces_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("summary.json");
        std::fs::write(&target, b"old").unwrap();

        let target_bytes_at_sync = std::cell::RefCell::new(Vec::new());
        write_bytes_atomic_with(
            &target,
            b"new",
            |_| {
                *target_bytes_at_sync.borrow_mut() = std::fs::read(&target).unwrap();
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();

        assert_eq!(
            target_bytes_at_sync.borrow().as_slice(),
            b"old",
            "the sync barrier must run on the temp file before the rename commits"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_syncs_parent_directory_after_the_rename_commits_a_create() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("summary.json");

        let events = std::cell::RefCell::new(Vec::new());
        write_bytes_atomic_with(
            &target,
            b"new",
            |_| {
                events.borrow_mut().push("sync_file");
                Ok(())
            },
            || {
                events.borrow_mut().push(
                    if std::fs::read(&target).is_ok_and(|bytes| bytes == b"new") {
                        "sync_parent_after_rename"
                    } else {
                        "sync_parent_before_rename"
                    },
                );
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            events.borrow().as_slice(),
            ["sync_file", "sync_parent_after_rename"],
            "the parent-directory sync must run after the rename commits the new entry"
        );
    }

    #[test]
    fn atomic_write_syncs_parent_directory_after_rename_even_when_replacing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("summary.json");
        std::fs::write(&target, b"old").unwrap();

        let parent_syncs = std::cell::Cell::new(0);
        write_bytes_atomic_with(
            &target,
            b"new",
            |_| Ok(()),
            || {
                parent_syncs.set(parent_syncs.get() + 1);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            parent_syncs.get(),
            1,
            "a retry after a failed create-barrier would otherwise skip the parent sync"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_retries_parent_sync_after_a_failed_create_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("summary.json");

        let first = write_bytes_atomic_with(
            &target,
            b"new",
            |_| Ok(()),
            || Err(io::Error::other("directory barrier failed")),
        );
        assert_eq!(first.unwrap_err().to_string(), "directory barrier failed");
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        let parent_syncs = std::cell::Cell::new(0);
        write_bytes_atomic_with(
            &target,
            b"newer",
            |_| Ok(()),
            || {
                parent_syncs.set(parent_syncs.get() + 1);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(parent_syncs.get(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), b"newer");
    }

    #[test]
    fn create_dir_all_durable_syncs_every_directory_gaining_an_entry_on_a_fresh_chain() {
        let root = tempfile::tempdir().unwrap();
        let group = root.path().join("sessions").join("cwd");
        let session = group.join("session-id");

        let synced = std::cell::RefCell::new(Vec::new());
        create_dir_all_durable_with(
            &session,
            |dir| std::fs::create_dir_all(dir),
            |dir| {
                synced.borrow_mut().push(dir.to_path_buf());
                Ok(())
            },
        )
        .unwrap();

        assert!(session.is_dir());
        assert_eq!(
            synced.borrow().as_slice(),
            [
                group.clone(),
                root.path().join("sessions"),
                root.path().to_path_buf()
            ],
            "each directory holding a newly created entry must be synced, up to the first pre-existing ancestor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_all_durable_fresh_create_skips_the_filesystem_root() {
        let unique = format!("grok-durable-create-{}", uuid::Uuid::now_v7());
        let top = Path::new("/").join(&unique);
        let dir = top.join("sessions").join("id");
        assert!(
            !top.exists(),
            "test unique top-level path must not already exist: {}",
            top.display()
        );

        let synced = std::cell::RefCell::new(Vec::new());
        create_dir_all_durable_with(
            &dir,
            |_dir| Ok(()),
            |path| {
                synced.borrow_mut().push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();

        let synced = synced.borrow();
        assert!(
            !synced.iter().any(|path| is_fs_root(path)),
            "a fresh create whose chain reaches a top-level directory must not fsync /, got {synced:?}"
        );
        assert!(
            synced.iter().any(|path| path == &top),
            "the new top-level directory itself must still be synced, got {synced:?}"
        );
        assert!(
            synced.iter().any(|path| path == &top.join("sessions")),
            "intermediate parents that gained an entry must still be synced, got {synced:?}"
        );
    }

    #[test]
    fn create_dir_all_durable_resyncs_ancestors_when_an_empty_chain_already_exists() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("sessions").join("cwd").join("session-id");
        std::fs::create_dir_all(&session).unwrap();

        let synced = std::cell::RefCell::new(Vec::new());
        create_dir_all_durable_with(
            &session,
            |dir| std::fs::create_dir_all(dir),
            |dir| {
                synced.borrow_mut().push(dir.to_path_buf());
                Ok(())
            },
        )
        .unwrap();

        let synced = synced.borrow();
        assert!(
            synced.iter().any(|path| path == session.parent().unwrap()),
            "a retry must re-sync the parent that holds the session direntry, got {synced:?}"
        );
        assert!(
            !synced.iter().any(|path| is_fs_root(path)),
            "must not fsync the filesystem root, got {synced:?}"
        );
    }

    #[test]
    fn create_dir_all_durable_occupied_existing_chain_pays_no_sync() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("sessions").join("cwd").join("session-id");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("summary.json"), b"{}").unwrap();

        let synced = std::cell::RefCell::new(Vec::new());
        create_dir_all_durable_with(
            &session,
            |dir| std::fs::create_dir_all(dir),
            |dir| {
                synced.borrow_mut().push(dir.to_path_buf());
                Ok(())
            },
        )
        .unwrap();

        assert!(
            synced.borrow().is_empty(),
            "resume of a populated session dir must not fsync ancestors, got {:?}",
            synced.borrow()
        );
    }

    #[test]
    fn create_dir_all_durable_retries_ancestor_syncs_after_a_failed_create_barrier() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("sessions").join("cwd").join("session-id");

        let first = create_dir_all_durable_with(
            &session,
            |dir| std::fs::create_dir_all(dir),
            |_| Err(io::Error::other("directory barrier failed")),
        );
        assert_eq!(first.unwrap_err().to_string(), "directory barrier failed");
        assert!(session.is_dir());

        let synced = std::cell::RefCell::new(Vec::new());
        create_dir_all_durable_with(
            &session,
            |dir| std::fs::create_dir_all(dir),
            |dir| {
                synced.borrow_mut().push(dir.to_path_buf());
                Ok(())
            },
        )
        .unwrap();

        assert!(
            synced
                .borrow()
                .iter()
                .any(|path| path == session.parent().unwrap()),
            "a retry after create+failed sync must still durableize the new direntry"
        );
    }

    #[test]
    fn atomic_write_parent_sync_error_propagates_and_keeps_the_committed_rename() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("summary.json");

        let error = write_bytes_atomic_with(
            &target,
            b"new",
            |_| Ok(()),
            || Err(io::Error::other("directory barrier failed")),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "directory barrier failed");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"new",
            "a failed directory barrier reports the error but never unwinds the committed rename"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_sync_barrier_error_propagates_keeps_target_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("summary.json");
        std::fs::write(&target, b"old").unwrap();

        let error = write_bytes_atomic_with(
            &target,
            b"new",
            |_| Err(io::Error::other("file barrier failed")),
            || Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "file barrier failed");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "a failed sync must remove its temp file"
        );
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Wrap an ACP notification as the envelope stored in updates.jsonl.
    fn acp_envelope(session_update_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"session/update","params":{{"sessionId":"s","update":{session_update_json}}}}}"#
        )
    }

    /// Wrap a Grok notification as the envelope stored in updates.jsonl.
    fn xai_envelope(session_update_json: &str) -> String {
        format!(
            r#"{{"timestamp":1,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{session_update_json}}}}}"#
        )
    }

    // ── parse_prompt_extract_event unit tests ─────────────────────────────────

    #[test]
    fn acp_user_text_chunk_yields_user_text() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::user_text("hello")
        );
    }

    #[test]
    fn acp_user_text_chunk_with_json_escapes_yields_user_text() {
        // Escaped JSON strings cannot be borrowed as &str; a regression to a borrowed peek field would drop this prompt from extraction
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"multi\nline \"quoted\" caf\u00e9"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::user_text("multi\nline \"quoted\" caf\u{e9}")
        );
        // An escaped bash command now parses too and must be excluded by the bash_command predicate (it used to be excluded by the parse failure)
        let bash = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"! echo \"hi\"","_meta":{"bash_command":"echo \"hi\""}}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&bash),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn acp_agent_message_chunk_yields_not_user() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"reply"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn acp_tool_result_yields_not_user() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"tool_result","toolCallId":"c1","content":[{"type":"text","text":"big output"}]}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn xai_rewind_marker_yields_rewind_to() {
        let line = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":3,"created_at":"2024-01-01"}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::RewindTo(3)
        );
    }

    #[test]
    fn xai_rewind_to_zero_yields_rewind_to_zero() {
        let line = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::RewindTo(0)
        );
    }

    #[test]
    fn xai_diff_review_yields_not_user() {
        let line = xai_envelope(r#"{"sessionUpdate":"diff_review","content":[]}"#);
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn acp_user_message_chunk_image_yields_not_user() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"image_url","url":"data:image/png;base64,abc"}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn malformed_json_yields_not_user() {
        assert_eq!(
            parse_prompt_extract_event("not json at all!!!"),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// Empty string: the iterator skips blanks, but a direct call must still classify conservatively.
    #[test]
    fn empty_string_yields_not_user() {
        assert_eq!(
            parse_prompt_extract_event(""),
            PromptExtractEvent::NotUserMessage
        );
    }

    #[test]
    fn unknown_json_object_yields_not_user() {
        assert_eq!(
            parse_prompt_extract_event(r#"{"foo":"bar"}"#),
            PromptExtractEvent::NotUserMessage
        );
    }

    /// Old sessions wrote `{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk",...}}` directly without the `method`/`params` envelope.
    #[test]
    fn legacy_format_user_message_chunk() {
        let line = r#"{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"legacy prompt"}}}"#;
        assert_eq!(
            parse_prompt_extract_event(line),
            PromptExtractEvent::user_text("legacy prompt")
        );
    }

    #[test]
    fn legacy_format_non_user_update() {
        let line = r#"{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#;
        assert_eq!(
            parse_prompt_extract_event(line),
            PromptExtractEvent::NotUserMessage
        );
    }

    // ── PromptExtractIterator integration tests via tempfile ──────────────────

    fn write_updates_file(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    fn collect_events(path: &std::path::Path) -> Vec<PromptExtractEvent> {
        PromptExtractIterator::open(path)
            .unwrap()
            .unwrap()
            .collect()
    }

    #[test]
    fn iterator_single_user_prompt() {
        let chunk = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello world"}}"#,
        );
        let other = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"reply"}}"#,
        );
        let f = write_updates_file(&[&chunk, &other]);

        let events = collect_events(f.path());
        assert_eq!(
            events.first(),
            Some(&PromptExtractEvent::user_text("hello world"))
        );
        assert_eq!(events.get(1), Some(&PromptExtractEvent::NotUserMessage));
    }

    #[test]
    fn iterator_multi_chunk_user_message() {
        let c1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"part1 "}}"#,
        );
        let c2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"part2"}}"#,
        );
        let end = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#,
        );
        let f = write_updates_file(&[&c1, &c2, &end]);

        let events = collect_events(f.path());
        assert_eq!(
            events.first(),
            Some(&PromptExtractEvent::user_text("part1 "))
        );
        assert_eq!(events.get(1), Some(&PromptExtractEvent::user_text("part2")));
        assert_eq!(events.get(2), Some(&PromptExtractEvent::NotUserMessage));
    }

    #[test]
    fn iterator_rewind_marker_truncates() {
        let chunk = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
        );
        let end = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"a1"}}"#,
        );
        let rewind = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        let f = write_updates_file(&[&chunk, &end, &rewind]);

        let events = collect_events(f.path());
        assert_eq!(events.first(), Some(&PromptExtractEvent::user_text("p1")));
        assert_eq!(events.get(1), Some(&PromptExtractEvent::NotUserMessage));
        assert_eq!(events.get(2), Some(&PromptExtractEvent::RewindTo(0)));
    }

    #[test]
    fn iterator_skips_blank_lines() {
        let chunk = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}"#,
        );
        let f = write_updates_file(&["", "   ", &chunk, ""]);

        let events = collect_events(f.path());
        assert_eq!(events.len(), 1);
        assert_eq!(
            events.first(),
            Some(&PromptExtractEvent::user_text("hello"))
        );
    }

    #[test]
    fn iterator_malformed_line_does_not_panic() {
        let bad = "this is not json !!!";
        let good = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"ok"}}"#,
        );
        let f = write_updates_file(&[bad, &good]);

        let events = collect_events(f.path());
        assert_eq!(events.len(), 2);
        assert_eq!(events.first(), Some(&PromptExtractEvent::NotUserMessage));
        assert_eq!(events.get(1), Some(&PromptExtractEvent::user_text("ok")));
    }

    #[test]
    fn iterator_nonexistent_file_returns_none() {
        let result =
            PromptExtractIterator::open(std::path::Path::new("/nonexistent/updates.jsonl"));
        assert!(result.unwrap().is_none());
    }

    /// Full round-trip: simulate a session with two user prompts, one rewind, then a new prompt.
    /// Assemble the events into prompts the same way `load_user_prompts_from_updates` does.
    #[test]
    fn full_round_trip_with_rewind() {
        // Turn 1: "first prompt"
        let u1a = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first "}}"#,
        );
        let u1b = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"prompt"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer1"}}"#,
        );
        // Turn 2: "second prompt"
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"second prompt"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer2"}}"#,
        );
        // Rewind to before turn 2 (keep 1 prompt)
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        // Turn 2 (after rewind): "new second prompt"
        let u3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"new second prompt"}}"#,
        );
        let a3 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer3"}}"#,
        );

        let f = write_updates_file(&[&u1a, &u1b, &a1, &u2, &a2, &rw, &u3, &a3]);

        let prompts =
            collect_prompts_from_events(PromptExtractIterator::open(f.path()).unwrap().unwrap());

        assert_eq!(prompts, vec!["first prompt", "new second prompt"]);
    }

    #[test]
    fn collect_prompts_ignores_unmarked_phantoms_when_markers_present() {
        let events = [
            PromptExtractEvent::user_text_pi("hi", 0),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("!pwd phantom"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("echo hello", 1),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("echo hi instead"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("ty ty", 2),
            PromptExtractEvent::NotUserMessage,
        ];
        let prompts = collect_prompts_from_events(events.into_iter());
        assert_eq!(prompts, vec!["hi", "echo hello", "ty ty"]);
    }

    #[test]
    fn collect_prompts_mixed_unmarked_prefix_then_markers() {
        let events = [
            PromptExtractEvent::user_text("old0"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("old1"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("new2", 2),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text("!pwd"),
            PromptExtractEvent::NotUserMessage,
            PromptExtractEvent::user_text_pi("new3", 3),
            PromptExtractEvent::NotUserMessage,
        ];
        let prompts = collect_prompts_from_events(events.into_iter());
        assert_eq!(prompts, vec!["old0", "old1", "new2", "new3"]);
    }

    #[test]
    fn parse_extracts_prompt_index_from_update_meta() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi"},"_meta":{"promptIndex":3}}"#,
        );
        assert_eq!(
            parse_prompt_extract_event(&line),
            PromptExtractEvent::user_text_pi("hi", 3)
        );
    }

    fn user_chunk(text: &str, prompt_index: Option<usize>) -> SessionUpdate {
        let mut chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
            text.to_string(),
        )));
        if let Some(pi) = prompt_index {
            chunk = chunk.meta(
                serde_json::json!({ "promptIndex": pi })
                    .as_object()
                    .cloned(),
            );
        }
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new("s"),
            acp::SessionUpdate::UserMessageChunk(chunk),
        )))
    }

    fn agent_chunk(text: &str) -> SessionUpdate {
        SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
            acp::SessionId::new("s"),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_string()),
            ))),
        )))
    }

    /// The fork copy classifies raw lines while replay parity tests classify typed updates.
    /// A divergence between the two classifiers would silently shift fork truncation boundaries.
    #[test]
    fn rewind_step_classifiers_agree_on_serialized_updates() {
        let rewind = SessionUpdate::Xai(Box::new(
            crate::extensions::notification::SessionNotification {
                session_id: acp::SessionId::new("s"),
                update: crate::extensions::notification::SessionUpdate::RewindMarker {
                    target_prompt_index: 2,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                },
                meta: None,
            },
        ));
        let host_turn_chunk = {
            let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                "host".to_string(),
            )))
            .meta(serde_json::json!({ "hostTurn": true }).as_object().cloned());
            SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
                acp::SessionId::new("s"),
                acp::SessionUpdate::UserMessageChunk(chunk),
            )))
        };
        for update in [
            user_chunk("plain", None),
            user_chunk("marked", Some(4)),
            host_turn_chunk,
            agent_chunk("agent"),
            rewind,
        ] {
            let envelope = SessionUpdateEnvelope::from_update(&update).unwrap();
            let line = serde_json::to_string(&envelope).unwrap();
            assert_eq!(
                rewind_step_for_line(&line),
                rewind_step_for_update(&update),
                "raw and typed classification must agree for {line}"
            );
        }
    }

    #[test]
    fn updates_truncate_ignores_unmarked_phantoms_when_markers_present() {
        let updates = vec![
            user_chunk("P0", Some(0)),
            agent_chunk("A0"),
            user_chunk("!pwd", None),
            agent_chunk("out"),
            user_chunk("P1", Some(1)),
            agent_chunk("A1"),
            user_chunk("P2", Some(2)),
            agent_chunk("A2"),
        ];
        // Keep through P1 (indices 0,1); cut at start of P2 run.
        let cut = truncate_for_prompt_by(&updates, 1, rewind_step_for_update);
        assert_eq!(cut, 6);
        let Some(cut_update) = updates.get(cut) else {
            panic!("expected update at cut {cut}: {updates:?}");
        };
        assert!(matches!(
            cut_update,
            SessionUpdate::Acp(n) if matches!(
                &n.update,
                acp::SessionUpdate::UserMessageChunk(c)
                    if matches!(&c.content, acp::ContentBlock::Text(t) if t.text == "P2")
            )
        ));
    }

    #[test]
    fn updates_truncate_splits_consecutive_marked_prompts_without_agent() {
        let updates: Vec<_> = (0..6)
            .map(|i| user_chunk(&format!("P{i}"), Some(i)))
            .collect();
        // Target 2 keeps turns 0 and 1; cut at P2 (index 2).
        assert_eq!(
            truncate_for_prompt_by(&updates, 1, rewind_step_for_update),
            2
        );
        assert_eq!(
            truncate_for_prompt_by(&updates, 2, rewind_step_for_update),
            3
        );
        assert_eq!(
            truncate_for_prompt_by(&updates, 5, rewind_step_for_update),
            6
        );
    }

    /// Mixed stream: unmarked runs before the first promptIndex still count.
    #[test]
    fn updates_truncate_mixed_unmarked_prefix_then_markers() {
        let updates = vec![
            user_chunk("old0", None),
            agent_chunk("A0"),
            user_chunk("old1", None),
            agent_chunk("A1"),
            user_chunk("new2", Some(2)),
            agent_chunk("A2"),
            user_chunk("!pwd", None),
            agent_chunk("out"),
            user_chunk("new3", Some(3)),
            agent_chunk("A3"),
        ];
        // Target 1 keeps old0 and old1; cut at new2
        assert_eq!(
            truncate_for_prompt_by(&updates, 1, rewind_step_for_update),
            4
        );
        // Target 2 keeps through A2 (and phantom run does not add a turn); cut at new3.
        assert_eq!(
            truncate_for_prompt_by(&updates, 2, rewind_step_for_update),
            8
        );
        assert_eq!(
            truncate_for_prompt_by(&updates, 0, rewind_step_for_update),
            2
        );
    }

    #[test]
    fn filter_rewind_mixed_unmarked_prefix_then_markers() {
        let o0 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old0"}}"#,
        );
        let a0 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A0"}}"#,
        );
        let o1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old1"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A1"}}"#,
        );
        let n2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"new2"},"_meta":{"promptIndex":2}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A2"}}"#,
        );
        let n3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"new3"},"_meta":{"promptIndex":3}}"#,
        );
        // Rewind to target 2: keep turns 0,1 (old0, old1); drop new2 and everything after
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":2,"created_at":"2024-01-01"}"#,
        );
        let after = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"after"},"_meta":{"promptIndex":2}}"#,
        );
        let lines = vec![
            o0.as_str(),
            a0.as_str(),
            o1.as_str(),
            a1.as_str(),
            n2.as_str(),
            a2.as_str(),
            n3.as_str(),
            rw.as_str(),
            after.as_str(),
        ];
        let kept = filter_rewind_lines(lines);
        let texts: Vec<&str> = kept
            .iter()
            .filter_map(|l| {
                if l.contains("\"text\":\"old0\"") {
                    Some("old0")
                } else if l.contains("\"text\":\"old1\"") {
                    Some("old1")
                } else if l.contains("\"text\":\"new2\"") {
                    Some("new2")
                } else if l.contains("\"text\":\"new3\"") {
                    Some("new3")
                } else if l.contains("\"text\":\"after\"") {
                    Some("after")
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts, vec!["old0", "old1", "after"]);
    }

    #[test]
    fn filter_rewind_ignores_unmarked_phantoms_when_markers_present() {
        let p0 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"P0"},"_meta":{"promptIndex":0}}"#,
        );
        let a0 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A0"}}"#,
        );
        let phantom = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"!pwd"}}"#,
        );
        let p1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"P1"},"_meta":{"promptIndex":1}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"A1"}}"#,
        );
        let p2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"P2"},"_meta":{"promptIndex":2}}"#,
        );
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":2,"created_at":"2024-01-01"}"#,
        );
        let after = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"after"},"_meta":{"promptIndex":2}}"#,
        );
        let lines = vec![
            p0.as_str(),
            a0.as_str(),
            phantom.as_str(),
            p1.as_str(),
            a1.as_str(),
            p2.as_str(),
            rw.as_str(),
            after.as_str(),
        ];
        let kept = filter_rewind_lines(lines);
        let texts: Vec<&str> = kept
            .iter()
            .filter_map(|l| {
                if l.contains("\"text\":\"P0\"") {
                    Some("P0")
                } else if l.contains("!pwd") {
                    Some("phantom")
                } else if l.contains("\"text\":\"P1\"") {
                    Some("P1")
                } else if l.contains("\"text\":\"P2\"") {
                    Some("P2")
                } else if l.contains("\"text\":\"after\"") {
                    Some("after")
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts, vec!["P0", "phantom", "P1", "after"]);
    }

    // ── filter_rewind_lines tests ────────────────────────────────────────────

    #[test]
    fn filter_rewind_removes_dead_branch() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp1"}}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"second"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp2"}}"#,
        );
        // Rewind to prompt 1 kills u2, a2
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        let u3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"replacement"}}"#,
        );
        let a3 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp3"}}"#,
        );

        let lines = vec![
            u1.as_str(),
            a1.as_str(),
            u2.as_str(),
            a2.as_str(),
            rw.as_str(),
            u3.as_str(),
            a3.as_str(),
        ];
        let result = filter_rewind_lines(lines);

        assert_eq!(result.len(), 4);
        assert!(result.first().is_some_and(|s| s.contains("first")));
        assert!(result.get(1).is_some_and(|s| s.contains("resp1")));
        assert!(result.get(2).is_some_and(|s| s.contains("replacement")));
        assert!(result.get(3).is_some_and(|s| s.contains("resp3")));
    }

    #[test]
    fn filter_rewind_ignores_a_malformed_middle_line() {
        let user_message_1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first"}}"#,
        );
        let agent_message_1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp1"}}"#,
        );
        let user_message_2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"second"}}"#,
        );
        let agent_message_2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp2"}}"#,
        );
        let rewind_to_1 = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        let torn = "{ torn, unparseable jsonl line";

        // The malformed line is kept but not counted as a prompt boundary, so the rewind still drops prompt 1
        let survivors = filter_rewind_lines(vec![
            user_message_1.as_str(),
            agent_message_1.as_str(),
            torn,
            user_message_2.as_str(),
            agent_message_2.as_str(),
            rewind_to_1.as_str(),
        ]);

        pretty_assertions::assert_eq!(
            survivors,
            vec![user_message_1.as_str(), agent_message_1.as_str(), torn]
        );
    }

    #[test]
    fn filter_rewind_to_zero_clears_all() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"only"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resp"}}"#,
        );
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2024-01-01"}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"fresh start"}}"#,
        );

        let lines = vec![u1.as_str(), a1.as_str(), rw.as_str(), u2.as_str()];
        let result = filter_rewind_lines(lines);

        assert_eq!(result.len(), 1);
        assert!(result.first().is_some_and(|s| s.contains("fresh start")));
    }

    #[test]
    fn filter_rewind_double_rewind() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r1"}}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p2"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r2"}}"#,
        );
        let u3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p3"}}"#,
        );
        let a3 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r3"}}"#,
        );
        // Rewind to prompt 2 kills p3/r3
        let rw1 = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":2,"created_at":"2024-01-01"}"#,
        );
        let u4 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p4"}}"#,
        );
        let a4 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r4"}}"#,
        );
        // Rewind to prompt 1 kills p2/r2/p4/r4
        let rw2 = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        let u5 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"final"}}"#,
        );

        let lines = vec![
            u1.as_str(),
            a1.as_str(),
            u2.as_str(),
            a2.as_str(),
            u3.as_str(),
            a3.as_str(),
            rw1.as_str(),
            u4.as_str(),
            a4.as_str(),
            rw2.as_str(),
            u5.as_str(),
        ];
        let result = filter_rewind_lines(lines);

        assert_eq!(result.len(), 3);
        assert!(result.first().is_some_and(|s| s.contains("p1")));
        assert!(result.get(1).is_some_and(|s| s.contains("r1")));
        assert!(result.get(2).is_some_and(|s| s.contains("final")));
    }

    /// The raw-line filter and the typed filter must truncate an identical rewind timeline to the same surviving updates, in the same order.
    #[test]
    fn filter_rewind_lines_and_updates_agree() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r1"}}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p2"}}"#,
        );
        let a2 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r2"}}"#,
        );
        let rw1 = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":2,"created_at":"2024-01-01"}"#,
        );
        let u3 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p3"}}"#,
        );
        let a3 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r3"}}"#,
        );
        let rw2 = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":1,"created_at":"2024-01-01"}"#,
        );
        let u4 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"final"}}"#,
        );

        let lines = vec![
            u1.as_str(),
            a1.as_str(),
            u2.as_str(),
            a2.as_str(),
            rw1.as_str(),
            u3.as_str(),
            a3.as_str(),
            rw2.as_str(),
            u4.as_str(),
        ];

        let ser = |u: &SessionUpdate| serde_json::to_string(u).unwrap();
        let via_lines: Vec<String> = filter_rewind_lines(lines.clone())
            .iter()
            .map(|l| ser(&SessionUpdateEnvelope::from_str(l).unwrap()))
            .collect();
        let typed: Vec<SessionUpdate> = lines
            .iter()
            .map(|l| SessionUpdateEnvelope::from_str(l).unwrap())
            .collect();
        let via_updates: Vec<String> = filter_rewind_updates(typed).iter().map(ser).collect();

        assert_eq!(via_lines, via_updates);
    }

    /// An out-of-range rewind target folds to `result.len()` (the `unwrap_or(result.len())` branch in `filter_rewind_by`).
    /// So truncation is a no-op and every survivor is kept.
    #[test]
    fn filter_rewind_out_of_range_target_keeps_all() {
        let u1 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p1"}}"#,
        );
        let a1 = acp_envelope(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"r1"}}"#,
        );
        // Only prompt index 0 exists; target 5 is out of range.
        let rw = xai_envelope(
            r#"{"sessionUpdate":"rewind_marker","target_prompt_index":5,"created_at":"2024-01-01"}"#,
        );
        let u2 = acp_envelope(
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"p2"}}"#,
        );

        let lines = vec![u1.as_str(), a1.as_str(), rw.as_str(), u2.as_str()];
        let result = filter_rewind_lines(lines);

        assert_eq!(result.len(), 3);
        assert!(result.first().is_some_and(|s| s.contains("p1")));
        assert!(result.get(1).is_some_and(|s| s.contains("r1")));
        assert!(result.get(2).is_some_and(|s| s.contains("p2")));
    }

    // ── collect_assistant_text / collect_tool_metadata tests ──────────────────

    #[test]
    fn collect_assistant_text_extracts_chunks() {
        let lines = vec![
            acp_envelope(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}"#,
            ),
            acp_envelope(
                r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}"#,
            ),
        ];
        let updates: Vec<_> = lines
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s).unwrap()))
            .collect();
        let result = collect_assistant_text(updates.into_iter());
        assert_eq!(result, vec!["hello world"]);
    }

    #[test]
    fn collect_assistant_text_caps_at_100k() {
        // Two 60k chunks with non-ASCII, separator, and truncation
        let chunk1 = "x".repeat(60_000) + "café"; // 60k + 5 bytes (café is 5 UTF-8 bytes)
        let chunk2 = "日本語".repeat(20_000); // 60k bytes (3 bytes per char)
        let lines = vec![
            acp_envelope(&format!(
                r#"{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{chunk1}"}}}}"#
            )),
            acp_envelope(&format!(
                r#"{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{chunk2}"}}}}"#
            )),
        ];
        let updates: Vec<_> = lines
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s).unwrap()))
            .collect();
        let result = collect_assistant_text(updates.into_iter());
        let total: usize = result.iter().map(|s| s.len()).sum();
        assert!(total <= 100_000, "got {total} chars");
        assert!(
            result.iter().any(|s| s.contains("café")),
            "non-ASCII should be preserved"
        );
    }

    #[test]
    fn collect_tool_metadata_extracts_title_and_paths() {
        let line = acp_envelope(
            r#"{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Read `/tmp/foo.rs`","kind":"read","locations":[{"path":"/tmp/foo.rs"}]}"#,
        );
        let updates: Vec<_> = vec![Ok(serde_json::from_str(&line).unwrap())];
        let result = collect_tool_metadata(updates.into_iter());
        assert!(result.contains(&"Read `/tmp/foo.rs`".to_string()));
        assert!(result.contains(&"/tmp/foo.rs".to_string()));
    }

    #[test]
    fn collect_tool_metadata_caps_at_200_calls() {
        let mut lines = Vec::new();
        for i in 0..250 {
            lines.push(acp_envelope(&format!(
                r#"{{"sessionUpdate":"tool_call","toolCallId":"tc{i}","title":"tool_{i}","kind":"exec","locations":[]}}"#,
            )));
        }
        let updates: Vec<_> = lines
            .into_iter()
            .map(|s| Ok(serde_json::from_str(&s).unwrap()))
            .collect();
        let result = collect_tool_metadata(updates.into_iter());
        // The calls have empty locations, so every collected entry is a title
        let titles: Vec<_> = result.iter().filter(|s| s.starts_with("tool_")).collect();
        assert_eq!(titles.len(), 200);
    }

    #[test]
    fn from_str_unknown_xai_variant_deserializes_via_envelope() {
        // Simulates an updates.jsonl line containing a removed variant (e.g. git_branch_update).
        // SessionUpdateEnvelope::from_str must not error; the Unknown catch-all absorbs it
        let line = xai_envelope(r#"{"sessionUpdate":"git_branch_update","branch":"main"}"#);
        let update = SessionUpdateEnvelope::from_str(&line).unwrap();
        match update {
            SessionUpdate::Xai(notif) => {
                assert_eq!(
                    notif.update,
                    crate::extensions::notification::SessionUpdate::Unknown
                );
            }
            SessionUpdate::Acp(_) => panic!("expected Xai variant"),
        }
    }

    #[test]
    fn from_str_known_xai_variant_still_works() {
        let line = xai_envelope(r#"{"sessionUpdate":"memory_flush_started"}"#);
        let update = SessionUpdateEnvelope::from_str(&line).unwrap();
        match update {
            SessionUpdate::Xai(notif) => {
                assert_eq!(
                    notif.update,
                    crate::extensions::notification::SessionUpdate::MemoryFlushStarted
                );
            }
            SessionUpdate::Acp(_) => panic!("expected Xai variant"),
        }
    }
}
