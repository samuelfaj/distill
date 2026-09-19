//! Bounded, versioned feedback drafts stored beside a session's other local files.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::text::{FALLBACK_TITLE, title_prefix};
use crate::{FeedbackDraftId, FeedbackFailureMode, FeedbackTaskCategory, FeedbackType};

pub const FEEDBACK_DRAFTS_FILENAME: &str = "feedback_drafts.json";
pub const FEEDBACK_DRAFTS_LOCK_FILENAME: &str = ".feedback_drafts.lock";
pub const FEEDBACK_DRAFTS_TEMP_PREFIX: &str = ".feedback_drafts.tmp-";
pub const FEEDBACK_DRAFT_IMAGES_DIRNAME: &str = "feedback_draft_images";

#[must_use]
pub fn is_feedback_draft_artifact_name(name: &std::ffi::OsStr) -> bool {
    name == std::ffi::OsStr::new(FEEDBACK_DRAFTS_FILENAME)
        || name == std::ffi::OsStr::new(FEEDBACK_DRAFTS_LOCK_FILENAME)
        || name == std::ffi::OsStr::new(FEEDBACK_DRAFT_IMAGES_DIRNAME)
        || name
            .to_str()
            .is_some_and(|name| name.starts_with(FEEDBACK_DRAFTS_TEMP_PREFIX))
}

/// Directory that holds composer images copied onto one local draft.
#[must_use]
pub fn feedback_draft_images_dir(session_dir: &Path, id: &FeedbackDraftId) -> PathBuf {
    session_dir
        .join(FEEDBACK_DRAFT_IMAGES_DIRNAME)
        .join(id.as_str())
}

/// Snapshot of the canonical draft file identity used to exclude private drafts from copies.
#[derive(Debug)]
pub struct FeedbackDraftArtifactSet {
    draft_path: PathBuf,
    initial_draft: Option<same_file::Handle>,
}

impl FeedbackDraftArtifactSet {
    #[must_use]
    pub fn for_session(session_dir: &Path) -> Self {
        let draft_path = session_dir.join(FEEDBACK_DRAFTS_FILENAME);
        let initial_draft = same_file::Handle::from_path(&draft_path).ok();
        Self {
            draft_path,
            initial_draft,
        }
    }

    /// Opens a regular file without following a final symlink and returns `None` for draft artifacts.
    /// Returns an I/O error when the path cannot be opened or inspected.
    pub fn open_non_artifact(&self, path: &Path) -> io::Result<Option<File>> {
        if path
            .file_name()
            .is_some_and(is_feedback_draft_artifact_name)
        {
            return Ok(None);
        }
        let file = open_regular_nofollow(path)?;
        let candidate = file.try_clone().and_then(same_file::Handle::from_file)?;
        let is_draft = self
            .initial_draft
            .as_ref()
            .is_some_and(|draft| draft == &candidate)
            || same_file::Handle::from_path(&self.draft_path).is_ok_and(|draft| draft == candidate);
        Ok((!is_draft).then_some(file))
    }
}

const SCHEMA_VERSION: u32 = 1;
const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_DRAFTS: usize = 1_000;
const MAX_TEXT_BYTES: usize = 64 * 1024;
// Every holder does one read, write+fsync, unlock (ms-scale), so a retry bounded at <= 40 ms total
// (5 attempts, 4 sleeps) clears the common race; past that the caller still sees `Busy`.
const LOCK_RETRY_ATTEMPTS: usize = 5;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

pub type Result<T> = std::result::Result<T, FeedbackStoreError>;

#[derive(Debug, thiserror::Error)]
pub enum FeedbackStoreError {
    #[error("feedback session directory is invalid: {path}: {source}")]
    InvalidSessionDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("inspect feedback path {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("open feedback draft lock {path}: {source}")]
    OpenLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("feedback draft store is busy")]
    Busy,
    #[error("lock feedback draft store: {0}")]
    Lock(#[source] io::Error),
    #[error("read feedback drafts {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("feedback drafts document is too large: {observed} bytes exceeds {cap}")]
    TooLarge { observed: usize, cap: usize },
    #[error("feedback drafts contain {observed} drafts, exceeding the limit of {cap}")]
    DraftCapacityExceeded { observed: usize, cap: usize },
    #[error("feedback store path is a symlink: {path}")]
    SymlinkPath { path: PathBuf },
    #[error("feedback store path is not a regular file: {path}")]
    NonFilePath { path: PathBuf },
    #[error("decode feedback drafts {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("encode feedback drafts: {source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported feedback drafts schema version {version}")]
    UnsupportedSchema { version: u32 },
    #[error("invalid feedback drafts document: {reason}")]
    InvalidDocument { reason: &'static str },
    #[error("feedback draft ID cannot be empty")]
    EmptyDraftId,
    #[error("duplicate feedback draft ID: {id}")]
    DuplicateDraftId { id: FeedbackDraftId },
    #[error("feedback draft revision must be positive for {id}")]
    InvalidRevision { id: FeedbackDraftId },
    #[error("feedback draft not found: {id}")]
    DraftNotFound { id: FeedbackDraftId },
    #[error("feedback draft title cannot be blank")]
    BlankTitle,
    #[error("feedback draft details cannot be blank")]
    BlankDetails,
    #[error("feedback draft title is too large: {observed} bytes exceeds {cap}")]
    TitleTooLarge { observed: usize, cap: usize },
    #[error("feedback draft details are too large: {observed} bytes exceeds {cap}")]
    DetailsTooLarge { observed: usize, cap: usize },
    #[error("feedback draft area is too large: {observed} bytes exceeds {cap}")]
    AreaTooLarge { observed: usize, cap: usize },
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch(#[source] std::time::SystemTimeError),
    #[error("system clock timestamp exceeds i64")]
    ClockOutOfRange,
    #[error("create feedback draft temporary file in {path}: {source}")]
    CreateTemp {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("write feedback draft temporary file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("persist feedback drafts to {path}: {source}")]
    Persist {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("sync feedback session directory {path}: {source}")]
    SyncDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeedbackDraftInput {
    /// Short summary shown in the draft list and sent before the details.
    pub title: String,
    /// Structured report details saved in the local draft.
    pub details: String,
    /// Optional product area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// Feedback classification.
    #[serde(rename = "type")]
    pub r#type: FeedbackType,
    /// Optional task category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_category: Option<FeedbackTaskCategory>,
    /// Optional model-behavior failure mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_mode: Option<FeedbackFailureMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedbackDraft {
    pub id: FeedbackDraftId,
    pub title: String,
    pub details: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<FeedbackType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_category: Option<FeedbackTaskCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_mode: Option<FeedbackFailureMode>,
    pub created_at: i64,
    pub revision: u64,
}

#[derive(Deserialize)]
struct FeedbackDraftWire {
    id: FeedbackDraftId,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    details: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    area: Option<String>,
    #[serde(default, rename = "type")]
    r#type: Option<FeedbackType>,
    #[serde(default)]
    task_category: Option<FeedbackTaskCategory>,
    #[serde(default)]
    failure_mode: Option<FeedbackFailureMode>,
    created_at: i64,
    revision: u64,
}

impl<'de> Deserialize<'de> for FeedbackDraft {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = FeedbackDraftWire::deserialize(deserializer)?;
        let (title, details) = match (wire.title, wire.details, wire.text) {
            (Some(title), Some(details), _) => (title, details),
            (Some(title), None, Some(text)) => (title, text),
            (Some(title), None, None) => (title, String::new()),
            (None, Some(details), _) => split_legacy_text(details),
            (None, None, Some(text)) => split_legacy_text(text),
            (None, None, None) => (String::new(), String::new()),
        };
        Ok(Self {
            id: wire.id,
            title,
            details,
            area: wire.area,
            r#type: wire.r#type,
            task_category: wire.task_category,
            failure_mode: wire.failure_mode,
            created_at: wire.created_at,
            revision: wire.revision,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum UpdateOutcome {
    Updated,
    NotFound,
}

#[derive(Debug, Serialize, Deserialize)]
struct FeedbackDraftDocument {
    schema_version: u32,
    drafts: Vec<FeedbackDraft>,
}

impl Default for FeedbackDraftDocument {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            drafts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeedbackDraftStore {
    session_dir: PathBuf,
    data_path: PathBuf,
    lock_path: PathBuf,
}

impl FeedbackDraftStore {
    #[must_use]
    pub fn new(session_dir: impl Into<PathBuf>) -> Self {
        let session_dir = session_dir.into();
        Self {
            data_path: session_dir.join(FEEDBACK_DRAFTS_FILENAME),
            lock_path: session_dir.join(FEEDBACK_DRAFTS_LOCK_FILENAME),
            session_dir,
        }
    }

    /// Appends an immutable draft and returns the exact persisted value.
    /// Returns [`FeedbackStoreError`] when the input or existing document is invalid, the store
    /// is locked by another process, or the atomic write cannot be completed.
    pub fn append(&self, input: FeedbackDraftInput) -> Result<FeedbackDraft> {
        validate_feedback_draft_input(&input)?;
        let _lock = self.acquire_lock()?;
        let mut document = self.load_document()?;
        if document.drafts.len() >= MAX_DRAFTS {
            return Err(FeedbackStoreError::DraftCapacityExceeded {
                observed: document.drafts.len().saturating_add(1),
                cap: MAX_DRAFTS,
            });
        }
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(FeedbackStoreError::ClockBeforeUnixEpoch)
            .and_then(|duration| {
                i64::try_from(duration.as_secs()).map_err(|_| FeedbackStoreError::ClockOutOfRange)
            })?;
        let draft = FeedbackDraft {
            id: uuid::Uuid::now_v7().to_string().into(),
            title: input.title,
            details: input.details,
            area: input.area,
            r#type: Some(input.r#type),
            task_category: input.task_category,
            failure_mode: input.failure_mode,
            created_at,
            revision: 1,
        };
        document.drafts.push(draft.clone());
        self.commit(&document)?;
        Ok(draft)
    }

    /// Appends a draft that has text but no taxonomy yet. Never sets type.
    /// Returns [`FeedbackStoreError`] when the title or details are invalid, the store is locked,
    /// or the atomic write cannot be completed.
    pub fn append_predraft(&self, title: &str, details: &str) -> Result<FeedbackDraft> {
        validate_title(title)?;
        validate_details(details)?;
        let _lock = self.acquire_lock()?;
        let mut document = self.load_document()?;
        if document.drafts.len() >= MAX_DRAFTS {
            return Err(FeedbackStoreError::DraftCapacityExceeded {
                observed: document.drafts.len().saturating_add(1),
                cap: MAX_DRAFTS,
            });
        }
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(FeedbackStoreError::ClockBeforeUnixEpoch)
            .and_then(|duration| {
                i64::try_from(duration.as_secs()).map_err(|_| FeedbackStoreError::ClockOutOfRange)
            })?;
        let draft = FeedbackDraft {
            id: uuid::Uuid::now_v7().to_string().into(),
            title: title.to_owned(),
            details: details.to_owned(),
            area: None,
            r#type: None,
            task_category: None,
            failure_mode: None,
            created_at,
            revision: 1,
        };
        document.drafts.push(draft.clone());
        self.commit(&document)?;
        Ok(draft)
    }

    /// Returns drafts in persisted order.
    /// Returns [`FeedbackStoreError`] when the store cannot be locked or its document is invalid.
    pub fn list(&self) -> Result<Vec<FeedbackDraft>> {
        let _lock = self.acquire_lock()?;
        Ok(self.load_document()?.drafts)
    }

    /// Returns the draft matching `id` from one locked snapshot.
    /// Returns [`FeedbackStoreError`] when the store cannot be locked or its document is invalid.
    pub fn get(&self, id: &FeedbackDraftId) -> Result<Option<FeedbackDraft>> {
        let _lock = self.acquire_lock()?;
        Ok(self
            .load_document()?
            .drafts
            .into_iter()
            .find(|draft| draft.id == *id))
    }

    /// Deletes the draft matching `id` without rewriting on a miss.
    /// Returns [`FeedbackStoreError`] when the store cannot be locked, its document is invalid,
    /// or the atomic write cannot be completed.
    pub fn delete(&self, id: &FeedbackDraftId) -> Result<DeleteOutcome> {
        let _lock = self.acquire_lock()?;
        let mut document = self.load_document()?;
        let Some(index) = document.drafts.iter().position(|draft| draft.id == *id) else {
            return Ok(DeleteOutcome::NotFound);
        };
        document.drafts.remove(index);
        self.commit(&document)?;
        let _ = std::fs::remove_dir_all(feedback_draft_images_dir(&self.session_dir, id));
        Ok(DeleteOutcome::Deleted)
    }

    /// Replaces title, details, and taxonomy on an existing draft. A missing id never appends.
    /// Returns [`FeedbackStoreError`] when the input is invalid, the store cannot be locked,
    /// its document is invalid, or the atomic write cannot be completed.
    pub fn update_from_input(
        &self,
        id: &FeedbackDraftId,
        input: FeedbackDraftInput,
    ) -> Result<UpdateOutcome> {
        validate_feedback_draft_input(&input)?;
        let _lock = self.acquire_lock()?;
        let mut document = self.load_document()?;
        let Some(draft) = document.drafts.iter_mut().find(|draft| draft.id == *id) else {
            return Ok(UpdateOutcome::NotFound);
        };
        let next_area = input.area.filter(|area| !area.trim().is_empty());
        let unchanged = draft.title == input.title
            && draft.details == input.details
            && draft.area == next_area
            && draft.r#type == Some(input.r#type)
            && draft.task_category == input.task_category
            && draft.failure_mode == input.failure_mode;
        if unchanged {
            return Ok(UpdateOutcome::Updated);
        }
        draft.title = input.title;
        draft.details = input.details;
        draft.area = next_area;
        draft.r#type = Some(input.r#type);
        draft.task_category = input.task_category;
        draft.failure_mode = input.failure_mode;
        draft.revision = draft.revision.saturating_add(1);
        self.commit(&document)?;
        Ok(UpdateOutcome::Updated)
    }

    fn acquire_lock(&self) -> Result<StoreLock> {
        validate_session_dir(&self.session_dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(&self.lock_path).map_err(|source| {
            map_open_path_error(&self.lock_path, &source).unwrap_or_else(|| {
                FeedbackStoreError::OpenLock {
                    path: self.lock_path.clone(),
                    source,
                }
            })
        })?;
        validate_opened_file(&self.lock_path, &file)?;
        #[cfg(unix)]
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .map_err(|source| FeedbackStoreError::OpenLock {
                path: self.lock_path.clone(),
                source,
            })?;
        for attempt in 1..=LOCK_RETRY_ATTEMPTS {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(StoreLock { file }),
                Err(error) if is_lock_contended(&error) => {
                    if attempt < LOCK_RETRY_ATTEMPTS {
                        std::thread::sleep(LOCK_RETRY_DELAY);
                    }
                }
                Err(error) => return Err(FeedbackStoreError::Lock(error)),
            }
        }
        Err(FeedbackStoreError::Busy)
    }

    fn load_document(&self) -> Result<FeedbackDraftDocument> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = match options.open(&self.data_path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(FeedbackDraftDocument::default());
            }
            Err(source) => {
                return Err(
                    map_open_path_error(&self.data_path, &source).unwrap_or_else(|| {
                        FeedbackStoreError::Read {
                            path: self.data_path.clone(),
                            source,
                        }
                    }),
                );
            }
        };
        let metadata = validate_opened_file(&self.data_path, &file)?;
        let observed = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if observed > MAX_DOCUMENT_BYTES {
            return Err(FeedbackStoreError::TooLarge {
                observed,
                cap: MAX_DOCUMENT_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(observed);
        file.take((MAX_DOCUMENT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| FeedbackStoreError::Read {
                path: self.data_path.clone(),
                source,
            })?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(FeedbackStoreError::TooLarge {
                observed: bytes.len(),
                cap: MAX_DOCUMENT_BYTES,
            });
        }
        let document: FeedbackDraftDocument =
            serde_json::from_slice(&bytes).map_err(|source| FeedbackStoreError::Decode {
                path: self.data_path.clone(),
                source,
            })?;
        validate_document(&document)?;
        Ok(document)
    }

    fn commit(&self, document: &FeedbackDraftDocument) -> Result<()> {
        validate_document(document)?;
        let bytes = serde_json::to_vec_pretty(document)
            .map_err(|source| FeedbackStoreError::Encode { source })?;
        let persisted_bytes = bytes.len().saturating_add(1);
        if persisted_bytes > MAX_DOCUMENT_BYTES {
            return Err(FeedbackStoreError::TooLarge {
                observed: persisted_bytes,
                cap: MAX_DOCUMENT_BYTES,
            });
        }
        let mut temp = tempfile::Builder::new()
            .prefix(FEEDBACK_DRAFTS_TEMP_PREFIX)
            .tempfile_in(&self.session_dir)
            .map_err(|source| FeedbackStoreError::CreateTemp {
                path: self.session_dir.clone(),
                source,
            })?;
        #[cfg(unix)]
        temp.as_file()
            .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .map_err(|source| FeedbackStoreError::Write {
                path: temp.path().to_path_buf(),
                source,
            })?;
        temp.write_all(&bytes)
            .and_then(|()| temp.write_all(b"\n"))
            .and_then(|()| temp.flush())
            .and_then(|()| temp.as_file().sync_all())
            .map_err(|source| FeedbackStoreError::Write {
                path: temp.path().to_path_buf(),
                source,
            })?;
        ensure_replaceable_destination(&self.data_path)?;
        temp.persist(&self.data_path)
            .map_err(|error| FeedbackStoreError::Persist {
                path: self.data_path.clone(),
                source: error.error,
            })?;
        sync_directory(&self.session_dir).map_err(|source| FeedbackStoreError::SyncDirectory {
            path: self.session_dir.clone(),
            source,
        })
    }
}

#[must_use]
struct StoreLock {
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn validate_session_dir(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| {
        FeedbackStoreError::InvalidSessionDirectory {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FeedbackStoreError::InvalidSessionDirectory {
            path: path.to_path_buf(),
            source: io::Error::other("not a directory"),
        });
    }
    Ok(())
}

fn validate_opened_file(path: &Path, file: &File) -> Result<std::fs::Metadata> {
    let metadata = file
        .metadata()
        .map_err(|source| FeedbackStoreError::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(FeedbackStoreError::NonFilePath {
            path: path.to_path_buf(),
        });
    }
    Ok(metadata)
}

fn map_open_path_error(path: &Path, source: &io::Error) -> Option<FeedbackStoreError> {
    std::fs::symlink_metadata(path)
        .ok()
        .and_then(|metadata| {
            if metadata.file_type().is_symlink() {
                Some(FeedbackStoreError::SymlinkPath {
                    path: path.to_path_buf(),
                })
            } else if !metadata.is_file() {
                Some(FeedbackStoreError::NonFilePath {
                    path: path.to_path_buf(),
                })
            } else {
                None
            }
        })
        .or_else(|| {
            is_symlink_open_error(source).then(|| FeedbackStoreError::SymlinkPath {
                path: path.to_path_buf(),
            })
        })
}

#[cfg(unix)]
fn is_symlink_open_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
fn is_symlink_open_error(_error: &io::Error) -> bool {
    false
}

fn ensure_replaceable_destination(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(FeedbackStoreError::SymlinkPath {
            path: path.to_path_buf(),
        }),
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(FeedbackStoreError::NonFilePath {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(FeedbackStoreError::Inspect {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_document(document: &FeedbackDraftDocument) -> Result<()> {
    if document.schema_version != SCHEMA_VERSION {
        return Err(FeedbackStoreError::UnsupportedSchema {
            version: document.schema_version,
        });
    }
    if document.drafts.len() > MAX_DRAFTS {
        return Err(FeedbackStoreError::DraftCapacityExceeded {
            observed: document.drafts.len(),
            cap: MAX_DRAFTS,
        });
    }
    let mut ids = HashSet::with_capacity(document.drafts.len());
    for draft in &document.drafts {
        validate_title(&draft.title)?;
        validate_details(&draft.details)?;
        validate_area(draft.area.as_deref())?;
        if draft.id.as_str().is_empty() {
            return Err(FeedbackStoreError::EmptyDraftId);
        }
        if !ids.insert(draft.id.as_str()) {
            return Err(FeedbackStoreError::DuplicateDraftId {
                id: draft.id.clone(),
            });
        }
        if draft.revision == 0 {
            return Err(FeedbackStoreError::InvalidRevision {
                id: draft.id.clone(),
            });
        }
    }
    Ok(())
}

/// Validates a draft body without persisting it.
/// Returns a title, details, or area validation error when the input violates the same limits
/// enforced by [`FeedbackDraftStore::append`].
pub(crate) fn validate_feedback_draft_input(input: &FeedbackDraftInput) -> Result<()> {
    validate_feedback_draft_send(input, false)
}

/// Send-path check. A live image is enough body when `details` is blank; a
/// draft with neither text nor images is still rejected.
pub fn validate_feedback_draft_send(input: &FeedbackDraftInput, has_images: bool) -> Result<()> {
    validate_title(&input.title)?;
    if has_images {
        if input.details.len() > MAX_TEXT_BYTES {
            return Err(FeedbackStoreError::DetailsTooLarge {
                observed: input.details.len(),
                cap: MAX_TEXT_BYTES,
            });
        }
    } else {
        validate_details(&input.details)?;
    }
    validate_area(input.area.as_deref())
}

fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty() {
        return Err(FeedbackStoreError::BlankTitle);
    }
    if title.len() > MAX_TEXT_BYTES {
        return Err(FeedbackStoreError::TitleTooLarge {
            observed: title.len(),
            cap: MAX_TEXT_BYTES,
        });
    }
    Ok(())
}

fn validate_area(area: Option<&str>) -> Result<()> {
    let Some(area) = area else {
        return Ok(());
    };
    if area.len() > MAX_TEXT_BYTES {
        return Err(FeedbackStoreError::AreaTooLarge {
            observed: area.len(),
            cap: MAX_TEXT_BYTES,
        });
    }
    Ok(())
}

fn validate_details(details: &str) -> Result<()> {
    if details.trim().is_empty() {
        return Err(FeedbackStoreError::BlankDetails);
    }
    if details.len() > MAX_TEXT_BYTES {
        return Err(FeedbackStoreError::DetailsTooLarge {
            observed: details.len(),
            cap: MAX_TEXT_BYTES,
        });
    }
    Ok(())
}

fn split_legacy_text(text: String) -> (String, String) {
    let Some((start, line)) = text
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line))
        })
        .find(|(_, line)| !line.trim().is_empty())
    else {
        return (FALLBACK_TITLE.to_owned(), String::new());
    };
    let trimmed_line = line.trim();
    let title = title_prefix(trimmed_line);
    let line_remainder = trimmed_line.get(title.len()..).unwrap_or("").trim_start();
    let following_lines = text
        .get(start + line.len()..)
        .unwrap_or("")
        .trim_start_matches(['\r', '\n']);
    let details = match (line_remainder.is_empty(), following_lines.is_empty()) {
        (false, false) => format!("{line_remainder}\n{following_lines}"),
        (false, true) => line_remainder.to_owned(),
        (true, false) => following_lines.to_owned(),
        (true, true) => return (FALLBACK_TITLE.to_owned(), trimmed_line.to_owned()),
    };
    (title.to_owned(), details)
}

/// Opens a regular file without following the final symlink or reparse point.
/// Returns an I/O error when the path cannot be opened or is not a regular file.
pub fn open_regular_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// Reads a whole regular file through [`open_regular_nofollow`], refusing (never truncating) anything over `cap`; the check is on the bytes actually read, so a stale size cannot slip a growing file past it.
pub fn read_regular_capped(path: &Path, cap: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_regular_nofollow(path)?
        .take((cap as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(io::Error::new(
            ErrorKind::FileTooLarge,
            "file exceeds the read cap",
        ));
    }
    Ok(bytes)
}

fn is_lock_contended(error: &io::Error) -> bool {
    error.kind() == ErrorKind::WouldBlock
        || (error.raw_os_error().is_some()
            && error.raw_os_error() == fs2::lock_contended_error().raw_os_error())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "draft_store_tests.rs"]
mod tests;
