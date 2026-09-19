// Modified for Distill by Samuel Fajreldines, 2026.
use super::{PersistedData, SessionUpdateEnvelope, StorageAdapter};
use crate::sampling::types::ChatRequestMessage;
use crate::sampling::{ContentPart, ConversationItem};
use crate::session::info::Info;
use crate::session::persistence::{CHAT_FORMAT_VERSION, Summary};
use crate::tools::todo::TodoState;
use agent_client_protocol as acp;
use async_trait::async_trait;
use distill_chat_state::StrictAppendAck;
use distill_workspace::session::file_state::RewindPoint;
use fs2::FileExt;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
mod copy;
#[derive(Clone)]
enum SessionDirMode {
    FromRoot(PathBuf),
    Explicit(PathBuf),
}
#[derive(Clone, Copy)]
pub(crate) enum AppendDurability {
    Buffered,
    Durable,
}
/// JSONL storage under `{root}/sessions/{url_encoded_cwd}/{session_id}/`.
#[derive(Clone)]
pub struct JsonlStorageAdapter {
    dir_mode: SessionDirMode,
    #[cfg(test)]
    update_append_probe: Option<std::sync::Arc<AppendProbe>>,
    #[cfg(test)]
    session_sync_probe: Option<std::sync::Arc<SessionSyncProbe>>,
    #[cfg(test)]
    file_sync_probe: Option<std::sync::Arc<FileSyncProbe>>,
    #[cfg(test)]
    parent_sync_probe: Option<std::sync::Arc<ParentSyncProbe>>,
}
#[cfg(test)]
type AppendProbe = dyn Fn(AppendDurability) -> io::Result<()> + Send + Sync;
#[cfg(test)]
type SessionSyncProbe = dyn Fn(super::SessionFileSet) -> io::Result<()> + Send + Sync;
#[cfg(test)]
type FileSyncProbe = dyn Fn() -> io::Result<()> + Send + Sync;
#[cfg(test)]
type ParentSyncProbe = dyn Fn() -> io::Result<()> + Send + Sync;
/// `Committed` means `write_all` succeeded: the record is in the file (at least the page cache) and a later fsync can still make it durable.
#[derive(Debug)]
enum AppendLineError {
    NotCommitted(io::Error),
    Committed(io::Error),
}
impl AppendLineError {
    fn into_io_error(self) -> io::Error {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error,
        }
    }
    fn into_update_error(self) -> super::AppendUpdateError {
        match self {
            Self::NotCommitted(error) => super::AppendUpdateError::NotCommitted(error),
            Self::Committed(error) => super::AppendUpdateError::Committed(error),
        }
    }
}
impl std::fmt::Display for AppendLineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) | Self::Committed(error) => error.fmt(formatter),
        }
    }
}
impl Default for JsonlStorageAdapter {
    fn default() -> Self {
        Self::new()
    }
}
impl JsonlStorageAdapter {
    pub fn new() -> Self {
        Self {
            dir_mode: SessionDirMode::FromRoot(crate::util::distill_home::distill_home()),
            #[cfg(test)]
            update_append_probe: None,
            #[cfg(test)]
            session_sync_probe: None,
            #[cfg(test)]
            file_sync_probe: None,
            #[cfg(test)]
            parent_sync_probe: None,
        }
    }
    pub fn with_root(root_dir: PathBuf) -> Self {
        Self {
            dir_mode: SessionDirMode::FromRoot(root_dir),
            #[cfg(test)]
            update_append_probe: None,
            #[cfg(test)]
            session_sync_probe: None,
            #[cfg(test)]
            file_sync_probe: None,
            #[cfg(test)]
            parent_sync_probe: None,
        }
    }
    /// Create an adapter that writes directly to `session_dir`, bypassing the `{root}/sessions/{cwd}/{id}/` path computation.
    /// Subagent child sessions use this (top-level dirs; only their metadata nests under the parent's session dir).
    pub fn with_explicit_session_dir(session_dir: PathBuf) -> Self {
        Self {
            dir_mode: SessionDirMode::Explicit(session_dir),
            #[cfg(test)]
            update_append_probe: None,
            #[cfg(test)]
            session_sync_probe: None,
            #[cfg(test)]
            file_sync_probe: None,
            #[cfg(test)]
            parent_sync_probe: None,
        }
    }
    #[cfg(test)]
    pub(crate) fn with_update_append_probe(
        session_dir: PathBuf,
        append_probe: impl Fn(AppendDurability) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            dir_mode: SessionDirMode::Explicit(session_dir),
            update_append_probe: Some(std::sync::Arc::new(append_probe)),
            session_sync_probe: None,
            file_sync_probe: None,
            parent_sync_probe: None,
        }
    }
    /// [`Self::with_update_append_probe`] plus a probe observing (and able to fail) every [`StorageAdapter::sync_session_files_selected`] barrier.
    /// That probe receives the file set the barrier flushes.
    #[cfg(test)]
    pub(crate) fn with_probes(
        session_dir: PathBuf,
        append_probe: impl Fn(AppendDurability) -> io::Result<()> + Send + Sync + 'static,
        session_sync_probe: impl Fn(super::SessionFileSet) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            dir_mode: SessionDirMode::Explicit(session_dir),
            update_append_probe: Some(std::sync::Arc::new(append_probe)),
            session_sync_probe: Some(std::sync::Arc::new(session_sync_probe)),
            file_sync_probe: None,
            parent_sync_probe: None,
        }
    }
    /// Fail (or observe) the per-append file barrier after `write_all`.
    #[cfg(test)]
    pub(crate) fn with_file_sync_probe(
        mut self,
        file_sync_probe: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.file_sync_probe = Some(std::sync::Arc::new(file_sync_probe));
        self
    }
    /// Fail (or observe) the first-create `summary.json` parent-directory sync.
    #[cfg(test)]
    pub(crate) fn with_parent_sync_probe(
        mut self,
        parent_sync_probe: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.parent_sync_probe = Some(std::sync::Arc::new(parent_sync_probe));
        self
    }
    /// Fork bootstrap uses this to load the copied parent conversation.
    pub fn load_chat_history_from_dir(
        &self,
        dir: &std::path::Path,
    ) -> std::io::Result<Vec<ConversationItem>> {
        let chat_file = dir.join(super::CHAT_HISTORY_FILE);
        self.read_chat_history_sync(chat_file, CHAT_FORMAT_VERSION)
    }
    /// Read a durable transcript snapshot while enforcing caller-owned input caps.
    pub(crate) fn load_chat_history_bounded_from_dir(
        &self,
        dir: &std::path::Path,
        max_bytes: usize,
        max_items: usize,
    ) -> std::io::Result<Vec<ConversationItem>> {
        let chat_file = dir.join(super::CHAT_HISTORY_FILE);
        self.read_chat_history_sync_bounded(chat_file, CHAT_FORMAT_VERSION, max_bytes, max_items)
    }
    fn session_dir(&self, info: &Info) -> PathBuf {
        match &self.dir_mode {
            SessionDirMode::FromRoot(root) => {
                crate::util::distill_home::sessions_cwd_dir_in(root, &info.cwd)
                    .join(info.id.to_string())
            }
            SessionDirMode::Explicit(dir) => dir.clone(),
        }
    }
    /// Create `info`'s session dir owner-only and durable.
    /// `FromRoot` also makes the `<encoded-cwd>` dir and the sessions root owner-only; `Explicit` parents are caller-owned.
    fn create_session_dir_owner_only(&self, info: &Info) -> io::Result<PathBuf> {
        let dir = self.session_dir(info);
        super::create_dir_all_durable(&dir, |dir| match &self.dir_mode {
            SessionDirMode::FromRoot(root) => {
                if let Ok(cwd_dir) =
                    crate::util::distill_home::ensure_sessions_cwd_dir_in(root, &info.cwd)
                {
                    super::sync_cwd_marker_if_present(&cwd_dir)?;
                }
                crate::util::distill_home::create_dir_all_owner_only(dir)
            }
            SessionDirMode::Explicit(_) => {
                crate::util::distill_home::create_dir_all_owner_only(dir)
            }
        })?;
        Ok(dir)
    }
    pub(super) fn updates_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::UPDATES_FILE)
    }
    fn chat_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::CHAT_HISTORY_FILE)
    }
    fn ensure_chat_history(&self, info: &Info, chat_format_version: u8) -> io::Result<()> {
        if chat_format_version != crate::session::persistence::CHAT_FORMAT_VERSION {
            return Ok(());
        }
        let chat_file = self.chat_file(info);
        if std::fs::metadata(&chat_file).map(|m| m.len()).unwrap_or(0) == 0 {
            super::chat_rebuild::rebuild_chat_history(&self.session_dir(info))?;
        }
        Ok(())
    }
    fn summary_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::SUMMARY_FILE)
    }
    fn summary_lock_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info)
            .join(format!("{}.lock", super::SUMMARY_FILE))
    }
    fn plan_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::PLAN_FILE)
    }
    fn plan_mode_state_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::PLAN_MODE_FILE)
    }
    fn signals_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::SIGNALS_FILE)
    }
    fn usage_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::USAGE_FILE)
    }
    fn announcement_state_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::ANNOUNCEMENT_STATE_FILE)
    }
    fn goal_mode_state_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join(super::GOAL_STATE_FILE)
    }
    fn workflows_dir(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join("workflows")
    }
    fn workflow_run_dir(&self, info: &Info, run_id: &str) -> io::Result<PathBuf> {
        crate::session::workflow::store::validate_run_id(run_id)?;
        Ok(self.workflows_dir(info).join(run_id))
    }
    fn workflow_run_state_file(&self, info: &Info, run_id: &str) -> io::Result<PathBuf> {
        Ok(self.workflow_run_dir(info, run_id)?.join("state.json"))
    }
    fn rewind_points_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join("rewind_points.jsonl")
    }
    fn feedback_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join("feedback.jsonl")
    }
    fn btw_history_file(&self, info: &Info) -> PathBuf {
        self.session_dir(info).join("btw_history.jsonl")
    }
    /// Both `list_sessions` (full scan) and `list_sessions_recent` (mtime-based tail) share this.
    fn scan_session_dirs(&self, cwd: Option<&str>) -> io::Result<Vec<PathBuf>> {
        let root_dir = match &self.dir_mode {
            SessionDirMode::FromRoot(root) => root,
            SessionDirMode::Explicit(_) => return Ok(Vec::new()),
        };
        crate::session::storage::relocation::RelocationView::load(root_dir)
            .and_then(|view| view.session_dirs(cwd))
            .map_err(io::Error::other)
    }
    fn list_sessions_sync(&self, cwd: Option<&str>) -> io::Result<Vec<Summary>> {
        let session_dirs = self.scan_session_dirs(cwd)?;
        let mut summaries = Vec::new();
        for session_dir in session_dirs {
            let summary_path = session_dir.join(super::SUMMARY_FILE);
            match std::fs::read(&summary_path) {
                Ok(bytes) => {
                    if let Ok(mut summary) = serde_json::from_slice::<Summary>(&bytes)
                        && !summary.is_hidden()
                    {
                        super::summary_write::repair_untagged_worktree_summary(
                            &mut summary,
                            &summary_path,
                            &session_dir.join(format!("{}.lock", super::SUMMARY_FILE)),
                        );
                        summaries.push(summary);
                    }
                }
                Err(_) => continue,
            }
        }
        summaries.sort_by_cached_key(|s| {
            (
                std::cmp::Reverse(s.last_active_at.unwrap_or(s.updated_at)),
                s.info.id.0.to_string(),
            )
        });
        Ok(summaries)
    }
    /// Extra `summary.json` reads allowed past `limit` while skipping hidden/headless rows.
    /// The slack lets a mixed store still fill a page without scanning a headless-dominated tree.
    const RECENT_LIST_READ_SLACK: usize = 8;
    /// Instead of reading every `summary.json` (expensive at scale, ~12K files), this stats each file to get its mtime.
    /// On a machine with ~12K sessions this reduces cold-boot `workspace_list` from ~3s to ~200ms.
    /// Final order among candidates uses `last_active_at` else `updated_at`.
    pub async fn list_sessions_recent(&self, limit: usize) -> io::Result<Vec<Summary>> {
        let adapter = self.clone();
        tokio::task::spawn_blocking(move || adapter.list_sessions_recent_sync(limit))
            .await
            .map_err(io::Error::other)?
    }
    fn list_sessions_recent_sync(&self, limit: usize) -> io::Result<Vec<Summary>> {
        let session_dirs = self.scan_session_dirs(None)?;
        let mut candidates: Vec<(PathBuf, std::time::SystemTime)> =
            Vec::with_capacity(session_dirs.len());
        for session_dir in session_dirs {
            let summary_path = session_dir.join(super::SUMMARY_FILE);
            if let Ok(meta) = std::fs::metadata(&summary_path)
                && let Ok(mtime) = meta.modified()
            {
                candidates.push((summary_path, mtime));
            }
        }
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        let max_reads = limit
            .saturating_mul(Self::RECENT_LIST_READ_SLACK)
            .max(limit);
        let mut summaries = Vec::with_capacity(limit.min(candidates.len()));
        for (reads, (summary_path, _)) in candidates.into_iter().enumerate() {
            if summaries.len() == limit || reads >= max_reads {
                break;
            }
            match std::fs::read(&summary_path) {
                Ok(bytes) => {
                    if let Ok(mut summary) = serde_json::from_slice::<Summary>(&bytes)
                        && !summary.is_hidden()
                        && !summary.is_headless()
                    {
                        let lock_path =
                            summary_path.with_file_name(format!("{}.lock", super::SUMMARY_FILE));
                        super::summary_write::repair_untagged_worktree_summary(
                            &mut summary,
                            &summary_path,
                            &lock_path,
                        );
                        if !summary.is_unused_optimistic_husk() {
                            summaries.push(summary);
                        }
                    }
                }
                Err(_) => continue,
            }
        }
        summaries.sort_by_cached_key(|s| {
            (
                std::cmp::Reverse(s.last_active_at.unwrap_or(s.updated_at)),
                s.info.id.0.to_string(),
            )
        });
        Ok(summaries)
    }
    async fn append_jsonl<T: serde::Serialize>(&self, path: PathBuf, data: &T) -> io::Result<()> {
        self.append_jsonl_with_durability(path, data, AppendDurability::Buffered)
            .await
    }
    async fn append_jsonl_with_durability<T: serde::Serialize>(
        &self,
        path: PathBuf,
        data: &T,
        durability: AppendDurability,
    ) -> io::Result<()> {
        let mut line =
            serde_json::to_vec(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        Self::append_jsonl_line_blocking(path, line, durability).await
    }
    async fn append_jsonl_line_blocking(
        path: PathBuf,
        line: Vec<u8>,
        durability: AppendDurability,
    ) -> io::Result<()> {
        tokio::task::spawn_blocking(move || Self::append_jsonl_line_sync(&path, line, durability))
            .await
            .map_err(io::Error::other)?
    }
    async fn sync_file_path_durable(path: PathBuf) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new().write(true).open(&path)?;
            Self::sync_file_durable(&file)
        })
        .await
        .map_err(io::Error::other)?
    }
    /// Append one JSONL record, healing a torn tail before writing.
    /// Appends are not crash-atomic: a process kill / `ENOSPC` mid-`write_all` leaves the file ending in a *partial* record with no trailing newline.
    /// Before writing, check the last byte: if it isn't `\n`, prepend one so the torn record is terminated as its own (single) corrupt line.
    fn append_jsonl_line_sync(
        path: &Path,
        line: Vec<u8>,
        durability: AppendDurability,
    ) -> io::Result<()> {
        Self::append_jsonl_line_sync_with(path, line, durability, Self::sync_file_durable, || {
            Self::sync_parent_directory(path)
        })
        .map_err(AppendLineError::into_io_error)
    }
    fn append_jsonl_line_sync_with(
        path: &Path,
        mut line: Vec<u8>,
        durability: AppendDurability,
        mut sync_file: impl FnMut(&std::fs::File) -> io::Result<()>,
        mut sync_parent: impl FnMut() -> io::Result<()>,
    ) -> Result<(), AppendLineError> {
        debug_assert!(line.ends_with(b"\n"), "JSONL record must end with \\n");
        let lock = Self::lock_append(path).map_err(AppendLineError::NotCommitted)?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .read(true)
                .create(true)
                .append(true)
                .open(path)
                .map_err(AppendLineError::NotCommitted)?;
            let len = file
                .metadata()
                .map_err(AppendLineError::NotCommitted)?
                .len();
            if len > 0 {
                file.seek(io::SeekFrom::Start(len - 1))
                    .map_err(AppendLineError::NotCommitted)?;
                let mut last = [0u8; 1];
                file.read_exact(&mut last)
                    .map_err(AppendLineError::NotCommitted)?;
                if last[0] != b'\n' {
                    tracing::warn!(
                        path = %path.display(),
                        "jsonl file has a torn trailing line (previous append crashed mid-write?); terminating it before appending"
                    );
                    line.insert(0, b'\n');
                }
            }
            file.write_all(&line)
                .map_err(AppendLineError::NotCommitted)?;
            file.flush().map_err(AppendLineError::Committed)?;
            if matches!(durability, AppendDurability::Durable) {
                sync_file(&file).map_err(AppendLineError::Committed)?;
                drop(file);
                sync_parent().map_err(AppendLineError::Committed)?;
            } else {
                drop(file);
            }
            Ok(())
        })();
        let _ = lock.unlock();
        result
    }
    async fn append_cwd_switch_with_bookkeeping(
        &self,
        info: &Info,
        message: &ConversationItem,
    ) -> Result<StrictAppendAck, super::AppendCwdSwitchError> {
        let path = self.chat_file(info);
        let mut line = serde_json::to_vec(message).map_err(|error| {
            super::AppendCwdSwitchError::NotCommitted(io::Error::new(
                io::ErrorKind::InvalidData,
                error,
            ))
        })?;
        line.push(b'\n');
        let generation = message
            .working_directory_switch_generation()
            .filter(|generation| *generation > 0)
            .ok_or_else(|| {
                super::AppendCwdSwitchError::NotCommitted(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "working-directory switch item must carry a nonzero generation",
                ))
            })?;
        let disposition = tokio::task::spawn_blocking(move || {
            Self::append_cwd_switch_line_sync_with(
                &path,
                line,
                generation,
                Self::sync_file_durable,
                || Self::sync_parent_directory(&path),
            )
        })
        .await
        .map_err(|error| super::AppendCwdSwitchError::NotCommitted(io::Error::other(error)))??;
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                record_activity: matches!(&disposition, StrictAppendAck::Appended),
                chat_messages: matches!(&disposition, StrictAppendAck::Appended)
                    .then_some(super::summary_write::CounterOp::Increment(1)),
                chat_format_version: Some(CHAT_FORMAT_VERSION),
                cwd_switch_bookkeeping_generation: Some(generation),
                ..Default::default()
            },
        )
        .await
        .map_err(|source| super::AppendCwdSwitchError::Committed {
            acknowledgement: disposition.clone(),
            source,
        })?;
        Self::sync_file_path_durable(self.summary_file(info))
            .await
            .map_err(|source| super::AppendCwdSwitchError::Committed {
                acknowledgement: disposition.clone(),
                source,
            })?;
        Ok(disposition)
    }
    fn find_cwd_switch_generation(
        path: &Path,
        generation: u64,
    ) -> io::Result<Option<ConversationItem>> {
        let contents = match std::fs::read(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(contents.split(|byte| *byte == b'\n').find_map(|line| {
            let item = serde_json::from_slice::<ConversationItem>(line).ok()?;
            (item.working_directory_switch_generation() == Some(generation)).then_some(item)
        }))
    }
    pub(crate) fn append_cwd_switch_line_sync_with(
        path: &Path,
        mut line: Vec<u8>,
        generation: u64,
        mut sync_file: impl FnMut(&std::fs::File) -> io::Result<()>,
        mut sync_parent: impl FnMut() -> io::Result<()>,
    ) -> Result<StrictAppendAck, super::AppendCwdSwitchError> {
        let lock = Self::lock_append(path).map_err(super::AppendCwdSwitchError::NotCommitted)?;
        let result = (|| {
            if let Some(authoritative) = Self::find_cwd_switch_generation(path, generation)
                .map_err(super::AppendCwdSwitchError::NotCommitted)?
            {
                return Ok(StrictAppendAck::AlreadyPresent(authoritative));
            }
            let mut file = OpenOptions::new()
                .read(true)
                .create(true)
                .append(true)
                .open(path)
                .map_err(super::AppendCwdSwitchError::NotCommitted)?;
            let len = file
                .metadata()
                .map_err(super::AppendCwdSwitchError::NotCommitted)?
                .len();
            if len > 0 {
                file.seek(io::SeekFrom::Start(len - 1))
                    .map_err(super::AppendCwdSwitchError::NotCommitted)?;
                let mut last = [0u8; 1];
                file.read_exact(&mut last)
                    .map_err(super::AppendCwdSwitchError::NotCommitted)?;
                if last[0] != b'\n' {
                    line.insert(0, b'\n');
                }
            }
            file.write_all(&line)
                .map_err(super::AppendCwdSwitchError::NotCommitted)?;
            file.flush()
                .map_err(|source| super::AppendCwdSwitchError::Committed {
                    acknowledgement: StrictAppendAck::Appended,
                    source,
                })?;
            sync_file(&file).map_err(|source| super::AppendCwdSwitchError::Committed {
                acknowledgement: StrictAppendAck::Appended,
                source,
            })?;
            drop(file);
            sync_parent().map_err(|source| super::AppendCwdSwitchError::Committed {
                acknowledgement: StrictAppendAck::Appended,
                source,
            })?;
            Ok(StrictAppendAck::Appended)
        })();
        let _ = lock.unlock();
        result
    }
    /// Lock tail healing, append, and barriers through `<target>.jsonl.lock`.
    /// Full-file [`Self::write_jsonl`] atomic-rename rewrites bypass this append-only lock.
    fn lock_append(path: &Path) -> io::Result<std::fs::File> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("jsonl.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    fn sync_file_durable(file: &std::fs::File) -> io::Result<()> {
        super::sync_file_durable(file)
    }
    fn sync_parent_directory(path: &Path) -> io::Result<()> {
        super::sync_parent_dir_durable(path)
    }
    /// Write a full JSONL file (rewriting all items), crash-atomically: serialize to a temp file then rename over the target.
    /// A crash / `ENOSPC` mid-write therefore can't truncate the existing file (e.g. lose `rewind_points.jsonl` history).
    async fn write_jsonl<T: serde::Serialize>(&self, path: PathBuf, items: &[T]) -> io::Result<()> {
        super::write_jsonl_atomic_async(&path, items).await
    }
    fn read_jsonl<T: serde::de::DeserializeOwned>(&self, path: PathBuf) -> io::Result<Vec<T>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let mut items = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let item: T = serde_json::from_str(line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            items.push(item);
        }
        Ok(items)
    }
    /// Append a session update to the updates.jsonl file, wrapping it in an envelope with timestamp.
    pub(super) async fn append_update_to_file(
        &self,
        path: PathBuf,
        update: &super::SessionUpdate,
        durability: AppendDurability,
    ) -> Result<(), super::AppendUpdateError> {
        #[cfg(test)]
        if let Some(append_probe) = &self.update_append_probe {
            append_probe(durability).map_err(super::AppendUpdateError::NotCommitted)?;
        }
        let envelope = SessionUpdateEnvelope::from_update(update).map_err(|e| {
            super::AppendUpdateError::NotCommitted(io::Error::new(io::ErrorKind::InvalidData, e))
        })?;
        let mut line = serde_json::to_vec(&envelope).map_err(|e| {
            super::AppendUpdateError::NotCommitted(io::Error::new(io::ErrorKind::InvalidData, e))
        })?;
        line.push(b'\n');
        #[cfg(test)]
        let file_sync_probe = self.file_sync_probe.clone();
        tokio::task::spawn_blocking(move || {
            Self::append_jsonl_line_sync_with(
                &path,
                line,
                durability,
                |file| {
                    #[cfg(test)]
                    if let Some(probe) = &file_sync_probe {
                        probe()?;
                    }
                    Self::sync_file_durable(file)
                },
                || Self::sync_parent_directory(&path),
            )
            .map_err(AppendLineError::into_update_error)
        })
        .await
        .map_err(|error| super::AppendUpdateError::NotCommitted(io::Error::other(error)))?
    }
    async fn append_update_with_bookkeeping(
        &self,
        info: &Info,
        update: &super::SessionUpdate,
        durability: AppendDurability,
    ) -> Result<(), super::AppendUpdateError> {
        self.append_update_to_file(self.updates_file(info), update, durability)
            .await?;
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                record_activity: true,
                messages: Some(super::summary_write::CounterOp::Increment(1)),
                ..Default::default()
            },
        )
        .await
        .map_err(super::AppendUpdateError::Committed)
    }
    /// The borrowing envelope and `&RawValue` avoid an intermediate `Value` allocation.
    /// Corruption-tolerant like [`Self::read_chat_history_sync`]: a torn line (crashed or racing append) is skipped with a warning.
    /// Updates are display/replay data appended non-atomically, so skipping beats failing the session load.
    fn read_updates_jsonl(&self, path: PathBuf) -> io::Result<Vec<super::SessionUpdate>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = std::fs::read(&path)?;
        let mut skipped_lines: usize = 0;
        let mut updates = Vec::new();
        for line in contents.split(|b| *b == b'\n') {
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            let parsed = std::str::from_utf8(line)
                .map_err(|e| e.to_string())
                .and_then(|s| SessionUpdateEnvelope::from_str(s).map_err(|e| e.to_string()));
            match parsed {
                Ok(update) => updates.push(update),
                Err(error) => {
                    skipped_lines += 1;
                    if skipped_lines == 1 {
                        tracing::warn!(
                            error = %error,
                            path = %path.display(),
                            "skipping unparseable updates.jsonl line (torn append?)"
                        );
                    }
                }
            }
        }
        if skipped_lines > 0 {
            tracing::warn!(
                skipped = skipped_lines,
                loaded = updates.len(),
                path = %path.display(),
                "skipped unparseable session update lines"
            );
        }
        Ok(updates)
    }
    /// A first `summary.json` create that renamed then failed its parent-dir sync leaves the file in place.
    /// Retrying `init_session` would otherwise load it as an existing session and never re-run that sync.
    /// Occupied dirs (chat/updates) already paid later parent syncs; skip them so a normal resume does not fsync (network home / macOS F_FULLFSYNC).
    fn sync_parent_if_first_create_summary(
        &self,
        info: &Info,
        summary_path: &Path,
    ) -> io::Result<()> {
        let lock_name = format!("{}.lock", super::SUMMARY_FILE);
        let only_first_create = std::fs::read_dir(self.session_dir(info))
            .map(|entries| {
                entries.flatten().all(|entry| {
                    matches!(
                        entry.file_name().to_str(),
                        Some(name) if name == super::SUMMARY_FILE || name == lock_name
                    )
                })
            })
            .unwrap_or(false);
        if !only_first_create {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(probe) = &self.parent_sync_probe {
            probe()?;
        }
        super::sync_parent_dir_durable(summary_path)
    }
    /// A plain `std::fs::write` truncates before writing, so a concurrent reader may see an empty file.
    /// Writing a temp file then renaming avoids this.
    /// Crate-visible so `new_with_explicit_dir` can stamp session kind before identity.
    pub(crate) fn write_summary_sync(&self, info: &Info, summary: &Summary) -> io::Result<()> {
        let summary_path = self.summary_file(info);
        let bytes = serde_json::to_vec_pretty(summary)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic(&summary_path, &bytes)
    }
    fn read_summary_sync(&self, info: &Info) -> io::Result<Summary> {
        let path = self.summary_file(info);
        let bytes = std::fs::read(&path)?;
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("summary.json is empty (0 bytes): {}", path.display()),
            ));
        }
        serde_json::from_slice::<Summary>(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    fn read_optional_json_sync<T: serde::de::DeserializeOwned>(
        &self,
        path: &Path,
    ) -> io::Result<Option<T>> {
        if !path.exists() {
            return Ok(None);
        }
        match std::fs::read_to_string(path) {
            Ok(s) if s.trim().is_empty() => Ok(None),
            Ok(s) => match serde_json::from_str::<T>(&s) {
                Ok(v) => Ok(Some(v)),
                Err(e) => {
                    tracing::warn!(?e, "failed parsing json; returning None");
                    Ok(None)
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(?e, "failed reading json; returning None");
                }
                Ok(None)
            }
        }
    }
    fn load_workflow_runs_sync(
        &self,
        info: &Info,
    ) -> io::Result<Vec<crate::session::workflow::store::RestoredWorkflowRun>> {
        use crate::session::workflow::store::{
            MAX_RESTORED_WORKFLOW_RUNS, MAX_WORKFLOW_ARGS_BYTES, MAX_WORKFLOW_MANIFEST_BYTES,
            read_bounded_nofollow,
        };
        let workflows_dir = self.workflows_dir(info);
        match std::fs::symlink_metadata(&workflows_dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Ok(Vec::new());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let mut entries: Vec<_> = std::fs::read_dir(&workflows_dir)?
            .filter_map(Result::ok)
            .take(MAX_RESTORED_WORKFLOW_RUNS.saturating_add(1))
            .collect();
        let entries_truncated = entries.len() > MAX_RESTORED_WORKFLOW_RUNS;
        entries.sort_by_key(|entry| entry.file_name());
        entries.truncate(MAX_RESTORED_WORKFLOW_RUNS);
        if entries_truncated {
            tracing::warn!(
                path = %workflows_dir.display(),
                limit = MAX_RESTORED_WORKFLOW_RUNS,
                "workflow restore run-count cap reached; ignoring remaining entries"
            );
        }
        let mut restored = Vec::new();
        for entry in entries {
            let run_dir = entry.path();
            let Ok(run_meta) = std::fs::symlink_metadata(&run_dir) else {
                continue;
            };
            if run_meta.file_type().is_symlink() || !run_meta.is_dir() {
                continue;
            }
            if std::fs::symlink_metadata(run_dir.join("cleared"))
                .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
            {
                continue;
            }
            let manifest_path = run_dir.join("state.json");
            let manifest = match read_bounded_nofollow(&manifest_path, MAX_WORKFLOW_MANIFEST_BYTES)
                .and_then(|bytes| {
                    serde_json::from_slice::<crate::session::workflow::store::WorkflowRunManifest>(
                        &bytes,
                    )
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
                }) {
                Ok(manifest) => manifest,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(path = %manifest_path.display(), %error, "skipping invalid workflow manifest");
                    continue;
                }
            };
            if !matches!(
                manifest.version,
                1..=crate::session::workflow::store::WORKFLOW_RUN_MANIFEST_VERSION
            ) || crate::session::workflow::store::validate_run_id(&manifest.state.run_id)
                .is_err()
                || run_dir.file_name().and_then(|name| name.to_str())
                    != Some(manifest.state.run_id.as_str())
            {
                tracing::warn!(path = %manifest_path.display(), "skipping unsupported or mismatched workflow manifest");
                continue;
            }
            let script_path = crate::session::workflow::store::script_revision_path(
                &run_dir,
                manifest.script_revision,
            );
            let script = match read_bounded_nofollow(
                &script_path,
                crate::session::workflow::registry::MAX_WORKFLOW_SOURCE_BYTES,
            )
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            }) {
                Ok(script) => script,
                Err(error) => {
                    tracing::warn!(path = %script_path.display(), %error, "skipping workflow with missing immutable script");
                    continue;
                }
            };
            let args_path = run_dir.join("args.json");
            let args = match read_bounded_nofollow(&args_path, MAX_WORKFLOW_ARGS_BYTES).and_then(
                |bytes| {
                    serde_json::from_slice(&bytes)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
                },
            ) {
                Ok(args) => args,
                Err(error) => {
                    tracing::warn!(path = %args_path.display(), %error, "skipping workflow with missing immutable args");
                    continue;
                }
            };
            let effort_path = run_dir.join("effort");
            let effort = match read_bounded_nofollow(
                &effort_path,
                crate::session::workflow::store::MAX_WORKFLOW_EFFORT_BYTES,
            ) {
                Ok(bytes) => {
                    match String::from_utf8(bytes)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
                        .and_then(|effort| {
                            let parsed = effort
                                .parse::<distill_sampling_types::ReasoningEffort>()
                                .map_err(|error| {
                                    io::Error::new(io::ErrorKind::InvalidData, error)
                                })?;
                            if effort != parsed.as_ref() {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "workflow effort is not canonical",
                                ));
                            }
                            Ok(parsed)
                        }) {
                        Ok(effort) => Some(effort),
                        Err(error) => {
                            tracing::warn!(path = %effort_path.display(), %error, "skipping workflow with invalid immutable effort");
                            continue;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => {
                    tracing::warn!(path = %effort_path.display(), %error, "skipping workflow with invalid immutable effort");
                    continue;
                }
            };
            restored.push(crate::session::workflow::store::RestoredWorkflowRun {
                manifest,
                script,
                args,
                effort,
            });
        }
        Ok(restored)
    }
    /// Load `chat_history.jsonl` across legacy and current item formats; skip unparseable lines instead of failing the whole session.
    /// Appends are not crash-atomic, so one torn or raced line must not brick the load; the raw file is quarantined once as `chat_history.jsonl.corrupt`.
    /// Legacy inline `reasoning` / `raw_output` is upgraded in memory to sibling items before the assistant; disk is not rewritten, and a skipped corrupt line emits no siblings.
    fn read_chat_history_sync(
        &self,
        path: PathBuf,
        chat_format_version: u8,
    ) -> io::Result<Vec<ConversationItem>> {
        self.read_chat_history_sync_bounded(path, chat_format_version, usize::MAX, usize::MAX)
    }
    fn read_chat_history_sync_bounded(
        &self,
        path: PathBuf,
        chat_format_version: u8,
        max_bytes: usize,
        max_items: usize,
    ) -> io::Result<Vec<ConversationItem>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file()
            || metadata.len() > u64::try_from(max_bytes).unwrap_or(u64::MAX)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("chat history is not a regular file within the {max_bytes}-byte limit"),
            ));
        }
        let read_limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut contents = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(&path)?
            .take(read_limit)
            .read_to_end(&mut contents)?;
        if contents.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("chat history exceeds the {max_bytes}-byte read limit"),
            ));
        }
        let mut sibling_btc_ids_seen: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut upgraded_reasoning_count: usize = 0;
        let mut upgraded_btc_count: usize = 0;
        let mut skipped_lines: usize = 0;
        let mut first_skipped: Option<(usize, String)> = None;
        let mut skip_line = |line_no: usize, error: String| {
            skipped_lines += 1;
            if first_skipped.is_none() {
                first_skipped = Some((line_no, error));
            }
        };
        let mut items = Vec::new();
        for (line_idx, line) in contents.split(|b| *b == b'\n').enumerate() {
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            let raw: serde_json::Value = match serde_json::from_slice(line) {
                Ok(raw) => raw,
                Err(e) => {
                    skip_line(line_idx + 1, e.to_string());
                    continue;
                }
            };
            let item_result = if chat_format_version >= CHAT_FORMAT_VERSION {
                serde_json::from_value::<ConversationItem>(raw.clone()).or_else(|e| {
                    serde_json::from_value::<ChatRequestMessage>(raw.clone())
                        .map(ConversationItem::from)
                        .map_err(|_| e)
                })
            } else {
                serde_json::from_value::<ChatRequestMessage>(raw.clone())
                    .map(ConversationItem::from)
                    .or_else(|e| {
                        serde_json::from_value::<ConversationItem>(raw.clone()).map_err(|_| e)
                    })
            };
            let item = match item_result {
                Ok(item) => item,
                Err(e) => {
                    skip_line(line_idx + 1, e.to_string());
                    continue;
                }
            };
            let siblings =
                distill_sampling_types::upgrade_legacy_reasoning(&raw, &mut sibling_btc_ids_seen);
            for sib in siblings {
                match &sib {
                    ConversationItem::Reasoning(_) => upgraded_reasoning_count += 1,
                    ConversationItem::BackendToolCall(_) => upgraded_btc_count += 1,
                    _ => {}
                }
                items.push(sib);
            }
            if let ConversationItem::BackendToolCall(b) = &item {
                sibling_btc_ids_seen.insert(b.id().to_string());
            }
            items.push(item);
            if items.len() > max_items {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("chat history exceeds the {max_items}-item limit"),
                ));
            }
        }
        let stripped = strip_invalid_images(&mut items);
        if first_skipped.is_some() || stripped > 0 {
            let quarantine = path.with_extension("jsonl.corrupt");
            if !quarantine.exists()
                && let Err(e) = std::fs::copy(&path, &quarantine)
            {
                tracing::warn!(
                    error = %e,
                    path = %quarantine.display(),
                    "failed to write chat history quarantine copy"
                );
            }
        }
        if let Some((first_line, first_error)) = first_skipped {
            tracing::warn!(
                skipped = skipped_lines,
                loaded = items.len(),
                first_line,
                first_error = %first_error,
                path = %path.display(),
                "skipped unparseable chat history lines (torn or interleaved \
                 append — crashed mid-write or concurrent writer?); loading \
                 the session without them, original preserved as *.corrupt"
            );
        }
        if stripped > 0 {
            tracing::warn!(
                count = stripped,
                path = %path.display(),
                "stripped invalid images from loaded chat history, original \
                 preserved as *.corrupt"
            );
        }
        if upgraded_reasoning_count > 0 || upgraded_btc_count > 0 {
            tracing::info!(
                upgraded_reasoning = upgraded_reasoning_count,
                upgraded_backend_tool_calls = upgraded_btc_count,
                "reconstructed legacy reasoning siblings from pre-sibling-split session"
            );
        }
        Ok(items)
    }
    /// Apply a typed [`SummaryPatch`](super::summary_write::SummaryPatch) to this session's `summary.json` under an exclusive sidecar lock.
    /// The read-modify-write thus serializes against every other writer (including a second persistence actor on reconnect, or another process).
    /// This is the only path live sessions use to mutate the summary.
    pub(crate) async fn apply_summary_patch(
        &self,
        info: &Info,
        patch: super::summary_write::SummaryPatch,
    ) -> io::Result<()> {
        self.apply_summary_patch_reporting(info, patch).await?;
        Ok(())
    }
    pub(crate) async fn stamp_session_identity_if_absent(
        &self,
        info: &Info,
        required_agent_id: Option<String>,
        mint: impl FnOnce(Option<&str>) -> crate::session::persistence::SessionIdentity + Send + 'static,
    ) -> io::Result<crate::session::persistence::SessionIdentity> {
        let summary_path = self.summary_file(info);
        let lock_path = self.summary_lock_file(info);
        tokio::task::spawn_blocking(move || {
            super::summary_write::stamp_session_identity_if_absent(
                &summary_path,
                &lock_path,
                required_agent_id.as_deref(),
                mint,
            )
        })
        .await
        .map_err(io::Error::other)?
    }
    /// Like [`Self::apply_summary_patch`], but returns whether a `generated_title_if_absent` was applied or a manual pin was cleared.
    /// (`reset_title_to_auto` clears the pin; see [`Summary::apply_patch`].)
    async fn apply_summary_patch_reporting(
        &self,
        info: &Info,
        patch: super::summary_write::SummaryPatch,
    ) -> io::Result<bool> {
        let summary_path = self.summary_file(info);
        let lock_path = self.summary_lock_file(info);
        tokio::task::spawn_blocking(move || {
            super::summary_write::apply_patch_locked(&summary_path, &lock_path, &patch)
        })
        .await
        .map_err(io::Error::other)?
    }
}
fn transform_session_id_in_update(
    update: super::SessionUpdate,
    new_id: &acp::SessionId,
) -> super::SessionUpdate {
    match update {
        super::SessionUpdate::Acp(mut notification) => {
            notification.session_id = new_id.clone();
            super::SessionUpdate::Acp(notification)
        }
        super::SessionUpdate::Xai(mut notification) => {
            notification.session_id = new_id.clone();
            super::SessionUpdate::Xai(notification)
        }
    }
}
/// Next `segment_NNN` index in `compaction_dir`: one past the highest existing segment, or 0 when none exist.
/// Resume-safe: the index is derived from disk, not memory.
async fn next_compaction_segment_index(compaction_dir: &std::path::Path) -> u64 {
    let Ok(mut entries) = tokio::fs::read_dir(compaction_dir).await else {
        return 0;
    };
    let mut next = 0u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(n) = entry
            .file_name()
            .to_str()
            .and_then(distill_compaction_transcript::parse_segment_index)
        {
            next = next.max(n + 1);
        }
    }
    next
}
#[async_trait]
impl StorageAdapter for JsonlStorageAdapter {
    async fn init_session(&self, info: &Info, model_id: acp::ModelId) -> io::Result<Summary> {
        self.create_session_dir_owner_only(info)?;
        let summary_path = self.summary_file(info);
        if Path::new(&summary_path).exists() {
            self.sync_parent_if_first_create_summary(info, &summary_path)?;
            tracing::info!("Loading existing session from JSONL");
            let bytes = tokio::fs::read(&summary_path).await?;
            let mut summary = serde_json::from_slice::<Summary>(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if summary.session_kind.is_none() || summary.worktree_label.is_none() {
                let lock_path = self.summary_lock_file(info);
                summary = tokio::task::spawn_blocking(move || {
                    super::summary_write::repair_untagged_worktree_summary(
                        &mut summary,
                        &summary_path,
                        &lock_path,
                    );
                    summary
                })
                .await
                .map_err(io::Error::other)?;
            }
            Ok(summary)
        } else {
            tracing::info!("Creating new session in JSONL");
            let mut summary = Summary::new(info, model_id)?;
            summary.sandbox_profile = distill_sandbox::configured_profile_name().map(String::from);
            self.write_summary_sync(info, &summary)?;
            Ok(summary)
        }
    }
    async fn update_session_title(&self, info: &Info, session_title: String) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                generated_title: Some(session_title),
                ..Default::default()
            },
        )
        .await
    }
    async fn stamp_session_identity(
        &self,
        info: &Info,
        identity: crate::session::persistence::SessionIdentity,
    ) -> io::Result<crate::session::persistence::SessionIdentity> {
        let required_agent_id = identity.agent_id.clone();
        self.stamp_session_identity_if_absent(info, Some(required_agent_id), move |_| identity)
            .await
    }
    async fn set_session_kind_if_absent(&self, info: &Info, kind: String) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                session_kind_if_absent: Some(kind),
                ..Default::default()
            },
        )
        .await
    }
    async fn set_generated_title_if_absent(
        &self,
        info: &Info,
        session_title: String,
    ) -> io::Result<bool> {
        self.apply_summary_patch_reporting(
            info,
            super::summary_write::SummaryPatch {
                generated_title_if_absent: Some(session_title),
                ..Default::default()
            },
        )
        .await
    }
    async fn regenerate_generated_title(
        &self,
        info: &Info,
        session_title: String,
    ) -> io::Result<bool> {
        self.apply_summary_patch_reporting(
            info,
            super::summary_write::SummaryPatch {
                generated_title_regenerate: Some(session_title),
                ..Default::default()
            },
        )
        .await
    }
    async fn reset_title_to_auto(&self, info: &Info) -> io::Result<bool> {
        self.apply_summary_patch_reporting(
            info,
            super::summary_write::SummaryPatch {
                reset_title_to_auto: true,
                ..Default::default()
            },
        )
        .await
    }
    async fn set_last_recap(&self, info: &Info, recap: Option<String>) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                last_recap: Some(recap),
                ..Default::default()
            },
        )
        .await
    }
    async fn set_last_turn_summary(
        &self,
        info: &Info,
        summary: Option<(String, String)>,
    ) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                last_turn_summary: Some(summary),
                ..Default::default()
            },
        )
        .await
    }
    async fn append_update(&self, info: &Info, update: &super::SessionUpdate) -> io::Result<()> {
        self.append_update_commit_aware(info, update)
            .await
            .map_err(super::AppendUpdateError::into_io_error)
    }
    async fn append_update_commit_aware(
        &self,
        info: &Info,
        update: &super::SessionUpdate,
    ) -> Result<(), super::AppendUpdateError> {
        self.append_update_with_bookkeeping(info, update, AppendDurability::Buffered)
            .await
    }
    async fn append_update_durable_commit_aware(
        &self,
        info: &Info,
        update: &super::SessionUpdate,
    ) -> Result<(), super::AppendUpdateError> {
        self.append_update_with_bookkeeping(info, update, AppendDurability::Durable)
            .await
    }
    async fn append_chat_message(&self, info: &Info, message: &ConversationItem) -> io::Result<()> {
        self.append_chat_message_commit_aware(info, message)
            .await
            .map_err(super::AppendChatError::into_io_error)
    }
    async fn append_chat_message_commit_aware(
        &self,
        info: &Info,
        message: &ConversationItem,
    ) -> Result<(), super::AppendChatError> {
        self.append_jsonl(self.chat_file(info), message)
            .await
            .map_err(super::AppendChatError::NotCommitted)?;
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                record_activity: true,
                chat_messages: Some(super::summary_write::CounterOp::Increment(1)),
                chat_format_version: Some(CHAT_FORMAT_VERSION),
                ..Default::default()
            },
        )
        .await
        .map_err(super::AppendChatError::Committed)
    }
    async fn append_cwd_switch_commit_aware(
        &self,
        info: &Info,
        message: &ConversationItem,
    ) -> Result<StrictAppendAck, super::AppendCwdSwitchError> {
        self.append_cwd_switch_with_bookkeeping(info, message).await
    }
    async fn update_current_model_and_agent(
        &self,
        info: &Info,
        model_id: &acp::ModelId,
        agent_name: Option<&str>,
        reasoning_effort: Option<Option<distill_sampling_types::ReasoningEffort>>,
    ) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                model: Some(super::summary_write::ModelPatch {
                    model_id: model_id.clone(),
                    agent_name: agent_name.map(String::from),
                    reasoning_effort,
                }),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_collection_id(&self, info: &Info, collection_id: &str) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                collection_id: Some(collection_id.to_string()),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_git_head(
        &self,
        info: &Info,
        commit: Option<String>,
        branch: Option<String>,
    ) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                git_head: Some(super::summary_write::GitHeadPatch { commit, branch }),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_next_trace_turn(
        &self,
        info: &Info,
        next_trace_turn: u64,
        request_id: Option<&str>,
    ) -> io::Result<()> {
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                trace_turn: Some(super::summary_write::TraceTurnPatch {
                    next_trace_turn,
                    request_id: request_id.map(String::from),
                }),
                ..Default::default()
            },
        )
        .await
    }
    async fn update_wake_start(
        &self,
        info: &Info,
        start: crate::session::persistence::WakeStart,
        abort: tokio_util::sync::CancellationToken,
    ) -> io::Result<()> {
        let summary_path = self.summary_file(info);
        let lock_path = self.summary_lock_file(info);
        tokio::task::spawn_blocking(move || {
            super::summary_write::update_wake_start_locked(&summary_path, &lock_path, start, &abort)
        })
        .await
        .map_err(io::Error::other)?
    }
    async fn restore_wake_summary(
        &self,
        info: &Info,
        prior: crate::session::persistence::WakeSummaryState,
    ) -> io::Result<()> {
        let summary_path = self.summary_file(info);
        let lock_path = self.summary_lock_file(info);
        tokio::task::spawn_blocking(move || {
            super::summary_write::restore_wake_summary_locked(&summary_path, &lock_path, prior)
        })
        .await
        .map_err(io::Error::other)?
    }
    async fn write_plan_state(&self, info: &Info, state: &TodoState) -> io::Result<()> {
        let state_json = serde_json::to_vec_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.plan_file(info), state_json).await
    }
    async fn write_plan_mode_state(
        &self,
        info: &Info,
        state: &crate::session::plan_mode::PlanModeSnapshot,
    ) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.plan_mode_state_file(info), json).await
    }
    async fn write_signals(
        &self,
        info: &Info,
        signals: &crate::session::signals::SessionSignals,
    ) -> io::Result<()> {
        let signals_json = serde_json::to_vec(signals)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.signals_file(info), signals_json).await
    }
    async fn read_usage(
        &self,
        info: &Info,
    ) -> io::Result<Option<crate::session::usage_file::SessionUsageFile>> {
        let path = self.usage_file(info);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if data.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    async fn write_usage(
        &self,
        info: &Info,
        usage: &crate::session::usage_file::SessionUsageFile,
    ) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(usage)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.usage_file(info), json).await
    }
    async fn write_announcement_state(
        &self,
        info: &Info,
        state: &crate::session::announcement_state::AnnouncementState,
    ) -> io::Result<()> {
        let json =
            serde_json::to_vec(state).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&self.announcement_state_file(info), json).await
    }
    async fn write_goal_mode_state(
        &self,
        info: &Info,
        state: &crate::session::goal_tracker::GoalOrchestration,
    ) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let target = self.goal_mode_state_file(info);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        super::write_bytes_atomic_async(&target, json).await
    }
    async fn delete_goal_mode_state(&self, info: &Info) -> io::Result<()> {
        match tokio::fs::remove_file(self.goal_mode_state_file(info)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
    async fn write_workflow_run_state(
        &self,
        info: &Info,
        manifest: &crate::session::workflow::store::WorkflowRunManifest,
    ) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(manifest)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let target = self.workflow_run_state_file(info, &manifest.state.run_id)?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
            if parent.join("cleared").is_file() {
                return Ok(());
            }
        }
        if target.is_file()
            && let Ok(existing) = tokio::fs::read(&target).await
            && let Ok(on_disk) = serde_json::from_slice::<
                crate::session::workflow::store::WorkflowRunManifest,
            >(&existing)
            && on_disk.state.run_id == manifest.state.run_id
            && on_disk.state.revision > manifest.state.revision
        {
            tracing::debug!(
                run_id = %manifest.state.run_id,
                on_disk_revision = on_disk.state.revision,
                incoming_revision = manifest.state.revision,
                "skipping stale workflow manifest write"
            );
            return Ok(());
        }
        let tmp = target.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        tokio::fs::write(&tmp, json).await?;
        #[cfg(windows)]
        match tokio::fs::remove_file(&target).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(error);
            }
        }
        if let Err(error) = tokio::fs::rename(&tmp, &target).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        Ok(())
    }
    async fn delete_workflow_run_state(&self, info: &Info, run_id: &str) -> io::Result<()> {
        let target = self.workflow_run_state_file(info, run_id)?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
            let cleared = parent.join("cleared");
            if !cleared.exists() {
                tokio::fs::write(cleared, []).await?;
            }
        }
        match tokio::fs::remove_file(target).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
    async fn load_session(&self, info: &Info) -> io::Result<PersistedData> {
        let summary = self.read_summary_sync(info)?;
        let chat_file = self.chat_file(info);
        self.ensure_chat_history(info, summary.chat_format_version)?;
        let chat_history = self.read_chat_history_sync(chat_file, summary.chat_format_version)?;
        let updates = self.read_updates_jsonl(self.updates_file(info))?;
        let plan_state = self.read_optional_json_sync::<TodoState>(&self.plan_file(info))?;
        let plan_mode_state = self
            .read_optional_json_sync::<crate::session::plan_mode::PlanModeSnapshot>(
                &self.plan_mode_state_file(info),
            )?;
        let signals = self.read_optional_json_sync::<crate::session::signals::SessionSignals>(
            &self.signals_file(info),
        )?;
        let announcement_state = self
            .read_optional_json_sync::<crate::session::announcement_state::AnnouncementState>(
                &self.announcement_state_file(info),
            )?;
        let goal_mode_state = self
            .read_optional_json_sync::<crate::session::goal_tracker::GoalOrchestration>(
                &self.goal_mode_state_file(info),
            )?;
        let workflow_runs = self.load_workflow_runs_sync(info)?;
        let rewind_points = self.read_jsonl::<RewindPoint>(self.rewind_points_file(info))?;
        let result = PersistedData {
            summary,
            chat_history,
            updates,
            plan_state,
            plan_mode_state,
            rewind_points,
            signals,
            announcement_state,
            goal_mode_state,
            workflow_runs,
        };
        tracing::info!(
            session_id = %info.id,
            num_chat_messages = result.chat_history.len(),
            num_updates = result.updates.len(),
            has_plan = result.plan_state.is_some(),
            has_signals = result.signals.is_some(),
            num_rewind_points = result.rewind_points.len(),
            chat_format_version = result.summary.chat_format_version,
            "Session data loaded successfully from JSONL"
        );
        Ok(result)
    }
    /// Resume path: loads everything except updates and rewind points.
    /// Rewind points can be huge (full file-content snapshots) and are needed only on an actual rewind.
    /// So they're deferred and loaded lazily by `FileStateTracker`.
    async fn load_session_without_updates(
        &self,
        info: &Info,
    ) -> io::Result<super::PersistedDataLight> {
        tracing::info!("Loading session data (without updates) from JSONL");
        let summary = self.read_summary_sync(info)?;
        let chat_file = self.chat_file(info);
        self.ensure_chat_history(info, summary.chat_format_version)?;
        let chat_history = self.read_chat_history_sync(chat_file, summary.chat_format_version)?;
        let plan_state = self.read_optional_json_sync::<TodoState>(&self.plan_file(info))?;
        let plan_mode_state = self
            .read_optional_json_sync::<crate::session::plan_mode::PlanModeSnapshot>(
                &self.plan_mode_state_file(info),
            )?;
        let signals = self.read_optional_json_sync::<crate::session::signals::SessionSignals>(
            &self.signals_file(info),
        )?;
        let announcement_state = self
            .read_optional_json_sync::<crate::session::announcement_state::AnnouncementState>(
                &self.announcement_state_file(info),
            )?;
        let goal_mode_state = self
            .read_optional_json_sync::<crate::session::goal_tracker::GoalOrchestration>(
                &self.goal_mode_state_file(info),
            )?;
        let workflow_runs = self.load_workflow_runs_sync(info)?;
        let result = super::PersistedDataLight {
            summary,
            chat_history,
            plan_state,
            plan_mode_state,
            signals,
            announcement_state,
            goal_mode_state,
            workflow_runs,
        };
        tracing::info!(
            session_id = %info.id,
            num_chat_messages = result.chat_history.len(),
            has_plan = result.plan_state.is_some(),
            has_signals = result.signals.is_some(),
            chat_format_version = result.summary.chat_format_version,
            "Session data loaded (without updates, rewind points deferred) from JSONL"
        );
        Ok(result)
    }
    async fn load_summary(&self, info: &Info) -> io::Result<Summary> {
        let info_clone = info.clone();
        let summary_handle = {
            let info = info_clone.clone();
            let adapter_clone = self.clone();
            tokio::task::spawn_blocking(move || {
                let adapter = adapter_clone;
                adapter.read_summary_sync(&info)
            })
        };
        let summary = summary_handle.await.map_err(io::Error::other)??;
        Ok(summary)
    }
    async fn list_sessions(&self, cwd: Option<&str>) -> io::Result<Vec<Summary>> {
        let adapter = self.clone();
        let cwd = cwd.map(str::to_owned);
        tokio::task::spawn_blocking(move || adapter.list_sessions_sync(cwd.as_deref()))
            .await
            .map_err(io::Error::other)?
    }
    async fn delete_session(&self, info: &Info) -> io::Result<()> {
        let dir = self.session_dir(info);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    async fn append_rewind_point(&self, info: &Info, point: &RewindPoint) -> io::Result<()> {
        self.append_jsonl(self.rewind_points_file(info), point)
            .await
    }
    async fn load_rewind_points(&self, info: &Info) -> io::Result<Vec<RewindPoint>> {
        let info_clone = info.clone();
        let adapter_clone = self.clone();
        tokio::task::spawn_blocking(move || {
            let adapter = adapter_clone;
            let path = adapter.rewind_points_file(&info_clone);
            adapter.read_jsonl::<RewindPoint>(path)
        })
        .await
        .map_err(io::Error::other)?
    }
    async fn truncate_rewind_points_from(&self, info: &Info, from_index: usize) -> io::Result<()> {
        let points = self.load_rewind_points(info).await?;
        let filtered: Vec<RewindPoint> = points
            .into_iter()
            .filter(|p| p.prompt_index < from_index)
            .collect();
        self.write_jsonl(self.rewind_points_file(info), &filtered)
            .await
    }
    async fn merge_rewind_points_from(&self, info: &Info, target_index: usize) -> io::Result<()> {
        let points = self.load_rewind_points(info).await?;
        let merged =
            distill_workspace::session::file_state::merge_rewind_points_from(points, target_index);
        self.write_jsonl(self.rewind_points_file(info), &merged)
            .await
    }
    async fn sync_session_files_selected(
        &self,
        info: &Info,
        files: super::SessionFileSet,
    ) -> io::Result<()> {
        let info_clone = info.clone();
        let adapter_clone = self.clone();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            use std::fs::OpenOptions;
            let adapter = adapter_clone;
            #[cfg(test)]
            if let Some(session_sync_probe) = &adapter.session_sync_probe {
                session_sync_probe(files)?;
            }
            let files_to_sync = [
                (files.updates, adapter.updates_file(&info_clone)),
                (files.chat, adapter.chat_file(&info_clone)),
                (files.summary, adapter.summary_file(&info_clone)),
                (files.plan, adapter.plan_file(&info_clone)),
                (files.rewind_points, adapter.rewind_points_file(&info_clone)),
            ];
            for (selected, file_path) in &files_to_sync {
                if !selected {
                    continue;
                }
                match OpenOptions::new().write(true).open(file_path) {
                    Ok(file) => super::sync_file_durable(&file)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            match super::sync_dir_durable(&adapter.session_dir(&info_clone)) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(io::Error::other)?
    }
    async fn backup_chat_history_before_strip(&self, info: &Info) -> io::Result<()> {
        let path = self.chat_file(info);
        let backup = path.with_extension("jsonl.pre-strip");
        if !tokio::fs::try_exists(&path).await? || tokio::fs::try_exists(&backup).await? {
            return Ok(());
        }
        let staging = path.with_extension("jsonl.pre-strip.tmp");
        tokio::fs::copy(&path, &staging).await?;
        tokio::fs::rename(&staging, &backup).await?;
        Ok(())
    }
    async fn replace_chat_history(
        &self,
        info: &Info,
        messages: &[ConversationItem],
    ) -> io::Result<()> {
        self.write_jsonl(self.chat_file(info), messages).await?;
        let new_count = messages.len();
        let cwd_switch_bookkeeping_generation = messages
            .iter()
            .filter_map(ConversationItem::working_directory_switch_generation)
            .max()
            .unwrap_or(0);
        self.apply_summary_patch(
            info,
            super::summary_write::SummaryPatch {
                chat_messages: Some(super::summary_write::CounterOp::Set(new_count)),
                chat_format_version: Some(CHAT_FORMAT_VERSION),
                cwd_switch_bookkeeping_generation: Some(cwd_switch_bookkeeping_generation),
                ..Default::default()
            },
        )
        .await
    }
    async fn copy_session_data(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: super::CopySessionOptions,
    ) -> io::Result<super::CopySessionResult> {
        let storage = self.clone();
        let source = source_info.clone();
        let target = target_info.clone();
        tokio::task::spawn_blocking(move || {
            storage.copy_session_data_sync(&source, &target, options)
        })
        .await
        .map_err(|e| io::Error::other(format!("spawn_blocking panicked: {e}")))?
    }
    async fn load_prompts_only(&self, info: &Info) -> io::Result<Vec<String>> {
        let updates_path = self.updates_file(info);
        if !updates_path.exists() {
            return Ok(Vec::new());
        }
        tokio::task::spawn_blocking(move || {
            let Some(iter) = super::PromptExtractIterator::open(&updates_path)? else {
                return Ok(Vec::new());
            };
            Ok(super::collect_prompts_from_events(iter))
        })
        .await
        .map_err(io::Error::other)?
    }
    #[tracing::instrument(skip_all, fields(session_id = %info.id))]
    async fn load_assistant_text(&self, info: &Info) -> io::Result<Vec<String>> {
        let updates_path = self.updates_file(info);
        if !updates_path.exists() {
            return Ok(Vec::new());
        }
        tokio::task::spawn_blocking(move || {
            let Some(iter) = super::UpdatesIterator::open(&updates_path)? else {
                return Ok(Vec::new());
            };
            Ok(super::collect_assistant_text(iter))
        })
        .await
        .map_err(io::Error::other)?
    }
    #[tracing::instrument(skip_all, fields(session_id = %info.id))]
    async fn load_tool_metadata(&self, info: &Info) -> io::Result<Vec<String>> {
        let updates_path = self.updates_file(info);
        if !updates_path.exists() {
            return Ok(Vec::new());
        }
        tokio::task::spawn_blocking(move || {
            let Some(iter) = super::UpdatesIterator::open(&updates_path)? else {
                return Ok(Vec::new());
            };
            Ok(super::collect_tool_metadata(iter))
        })
        .await
        .map_err(io::Error::other)?
    }
    fn updates_file_path(&self, info: &Info) -> Option<std::path::PathBuf> {
        Some(self.updates_file(info))
    }
    fn rewind_points_file_path(&self, info: &Info) -> Option<std::path::PathBuf> {
        Some(self.rewind_points_file(info))
    }
    async fn append_feedback(
        &self,
        info: &Info,
        entry: &crate::session::persistence::LocalFeedbackEntry,
    ) -> io::Result<()> {
        let path = self.feedback_file(info);
        self.append_jsonl(path, entry).await
    }
    async fn append_btw(
        &self,
        info: &Info,
        entry: &crate::session::persistence::BtwEntry,
    ) -> io::Result<()> {
        let path = self.btw_history_file(info);
        self.append_jsonl(path, entry).await
    }
    async fn write_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint: &crate::extensions::notification::CompactionCheckpointFile,
    ) -> io::Result<()> {
        let dir = self.session_dir(info).join("compaction_checkpoints");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", checkpoint.checkpoint_id));
        let bytes = serde_json::to_vec_pretty(checkpoint)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        super::write_bytes_atomic_async(&path, bytes).await
    }
    async fn write_compaction_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::CompactionRequestFile,
    ) -> io::Result<()> {
        let dir = self.session_dir(info).join("compaction_requests");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", request.request_id));
        let bytes = serde_json::to_vec_pretty(request)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(path, bytes).await
    }
    async fn write_recap_request(
        &self,
        info: &Info,
        request: &crate::extensions::notification::RecapRequestFile,
    ) -> io::Result<()> {
        let dir = self.session_dir(info).join("recap_requests");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.json", request.request_id));
        let bytes = serde_json::to_vec_pretty(request)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(path, bytes).await
    }
    async fn write_compaction_segment(
        &self,
        info: &Info,
        segment: &crate::extensions::notification::CompactionSegmentFile,
    ) -> io::Result<()> {
        use distill_compaction_transcript::{
            COMPACTION_DIR, INDEX_FILE, INDEX_HEADER, extract_keywords, render_index_row,
            render_segment_md, segment_filename,
        };
        use tokio::io::AsyncWriteExt;
        let base = self.session_dir(info).join(COMPACTION_DIR);
        tokio::fs::create_dir_all(&base).await?;
        let index = next_compaction_segment_index(&base).await;
        let md = render_segment_md(
            &segment.items,
            &segment.summary,
            index,
            segment.detail,
            &segment.timestamp,
        );
        tokio::fs::write(base.join(segment_filename(index)), md.as_bytes()).await?;
        let index_path = base.join(INDEX_FILE);
        let needs_header = !tokio::fs::try_exists(&index_path).await.unwrap_or(false);
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)
            .await?;
        if needs_header {
            f.write_all(INDEX_HEADER.as_bytes()).await?;
        }
        let keywords = extract_keywords(&segment.summary);
        let row = render_index_row(index, segment.items.len(), md.len(), &keywords);
        f.write_all(row.as_bytes()).await?;
        f.flush().await?;
        Ok(())
    }
    async fn read_compaction_checkpoint(
        &self,
        info: &Info,
        checkpoint_file: &str,
    ) -> io::Result<crate::extensions::notification::CompactionCheckpointFile> {
        let path = self.session_dir(info).join(checkpoint_file);
        let bytes = tokio::fs::read(&path).await?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}
/// Max decoded size for a data-URI image loaded from persisted history.
/// Generous (20 MB): fresh images use 5 MB, but loaded ones just need sanity-checking.
const MAX_LOADED_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Strip data-URI images the API would reject from loaded conversation items, so a poisoned history recovers instead of 400ing on every turn.
/// The verdicts come from [`persisted_image_reject_reason`](crate::session::image_normalize::persisted_image_reject_reason).
/// User parts become a text placeholder; `ToolResultItem.images` entries are removed.
pub(crate) fn strip_invalid_images(items: &mut [ConversationItem]) -> usize {
    fn invalid(part: &ContentPart) -> bool {
        match part {
            ContentPart::Image { url } => url.starts_with("data:") && !is_valid_data_uri_image(url),
            _ => false,
        }
    }
    let mut stripped = 0usize;
    for item in items.iter_mut() {
        match item {
            ConversationItem::User(user) => {
                for part in user.content.iter_mut() {
                    if invalid(part) {
                        *part = ContentPart::Text {
                            text: std::sync::Arc::<str>::from(
                                "[image removed \u{2014} invalid data]",
                            ),
                        };
                        stripped += 1;
                    }
                }
            }
            ConversationItem::ToolResult(t) => {
                let before = t.images.len();
                t.images.retain(|part| !invalid(part));
                stripped += before - t.images.len();
            }
            _ => {}
        }
    }
    stripped
}
/// Check that a `data:` URI has a valid `;base64,` header and decodable payload within the size limit.
fn is_valid_data_uri_image(url: &str) -> bool {
    use base64::Engine as _;
    let after_data = match url.strip_prefix("data:") {
        Some(s) => s,
        None => return false,
    };
    let comma = match after_data.find(',') {
        Some(i) => i,
        None => return false,
    };
    let Some(header) = after_data.get(..comma) else {
        return false;
    };
    let Some(payload) = after_data.get(comma + 1..) else {
        return false;
    };
    if !header
        .as_bytes()
        .windows(7)
        .any(|w| w.eq_ignore_ascii_case(b";base64"))
    {
        return false;
    }
    if payload.len() * 3 / 4 > MAX_LOADED_IMAGE_BYTES {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload) else {
        return false;
    };
    match crate::session::image_normalize::persisted_image_reject_reason(&bytes) {
        None => true,
        Some(reason) => {
            tracing::warn!(reason, "stripping unsendable image from loaded history");
            false
        }
    }
}
#[cfg(test)]
mod durable_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod worktree_heal_tests;
