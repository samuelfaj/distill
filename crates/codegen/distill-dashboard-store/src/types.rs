//! Typed values for the workspace store: identifiers, enums stored as raw text, member rows, and write payloads.
//!
//! Validation follows "parse, don't validate": [`SessionId::new`] is the single validation point for ids.
//! Every API taking a `SessionId` is therefore injection-safe by type.
//! Enum values are typed in code but stored as raw text; values this binary does not know decode as `Other` and are written back byte-identical.

use std::fmt;

use crate::error::{Result, StoreError};

/// Maximum number of workspace members.
/// UI copy about the limit must derive from this constant.
pub const WORKSPACE_CAPACITY: usize = 256;

/// Rank spacing for `pin_rank` / `order_rank`.
/// Appending assigns `max_rank + RANK_GAP`; inserting between neighbors `a < b` takes `(a + b) / 2` (integer midpoint).
pub const RANK_GAP: i64 = 1024;

/// Byte cap for [`SessionId`] (one path component, like any filename).
pub const MAX_SESSION_ID_BYTES: usize = 255;
/// Byte cap for `cwd`.
/// A longer path errors rather than truncates: a clipped path is corruption, not display damage.
pub const MAX_CWD_BYTES: usize = 4096;
/// Byte cap for `title`; over-long values truncate at a char boundary.
pub const MAX_TITLE_BYTES: usize = 1024;
/// Byte cap for `model`; over-long values truncate at a char boundary.
pub const MAX_MODEL_BYTES: usize = 256;
/// Defensive byte cap for `last_turn_summary`; over-long values truncate at a char boundary.
pub const MAX_SUMMARY_BYTES: usize = 8192;
/// Byte cap for caller-supplied unknown enum values entering through `from_raw`.
/// Values over the cap are rejected, never truncated; values read back from the store are exempt so passthrough stays unconditional.
pub const MAX_ENUM_BYTES: usize = 64;

/// One portable path component: non-empty, at most [`MAX_SESSION_ID_BYTES`] bytes, and valid as a file name on Unix and Windows.
/// Constructed once at the boundary, so downstream joins of ids into filesystem paths cannot traverse or alias another id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// # Errors
    ///
    /// [`StoreError::InvalidSessionId`] when `raw` is empty, over [`MAX_SESSION_ID_BYTES`] bytes, or is not a portable path component.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        let reason = if raw.is_empty() {
            Some("empty")
        } else if raw.len() > MAX_SESSION_ID_BYTES {
            Some("longer than the byte cap")
        } else if raw == "." || raw == ".." {
            Some("reserved path component")
        } else if raw.contains(['/', '\\']) {
            Some("contains a path separator")
        } else if raw.contains(['<', '>', ':', '"', '|', '?', '*']) {
            Some("contains a character Windows forbids in file names")
        } else if raw.ends_with(['.', ' ']) {
            Some("has a trailing dot or space")
        } else if is_windows_reserved_name(&raw) {
            Some("is a reserved Windows device name")
        } else if raw.chars().any(is_identifier_control) {
            Some("contains a control character")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(StoreError::InvalidSessionId { reason }),
            None => Ok(Self(raw)),
        }
    }
}

fn is_windows_reserved_name(raw: &str) -> bool {
    let stem_end = raw.find('.').unwrap_or(raw.len());
    let Some(stem) = raw.get(..stem_end) else {
        return false;
    };
    let stem = stem.trim_end_matches(' ');
    if ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return true;
    }

    let Some(prefix) = stem.get(..3) else {
        return false;
    };
    let mut suffix = stem.get(3..).unwrap_or("").chars();
    (prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
        && matches!(suffix.next(), Some('1'..='9' | '¹' | '²' | '³'))
        && suffix.next().is_none()
}

fn is_identifier_control(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = StoreError;

    fn try_from(raw: String) -> Result<Self> {
        Self::new(raw)
    }
}

fn validate_unknown(column: &'static str, raw: &str) -> Result<()> {
    if raw.is_empty() {
        return Err(StoreError::InvalidEnumValue {
            column,
            reason: "empty",
        });
    }
    if raw.len() > MAX_ENUM_BYTES {
        return Err(StoreError::EnumValueTooLong {
            column,
            max: MAX_ENUM_BYTES,
        });
    }
    Ok(())
}

macro_rules! unknown_value {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            fn parsed(column: &'static str, raw: &str) -> Result<Self> {
                validate_unknown(column, raw)?;
                Ok(Self(raw.to_owned()))
            }

            fn stored(raw: String) -> Self {
                Self(raw)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

unknown_value!(UnknownMemberKind);
unknown_value!(UnknownMemberOrigin);
unknown_value!(UnknownGrouping);

/// What a workspace member is; half the primary key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemberKind {
    Build,
    Conversation,
    /// A value written by a newer binary; round-trips unchanged.
    Other(UnknownMemberKind),
}

impl MemberKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Build => "build",
            Self::Conversation => "conversation",
            Self::Other(value) => value.as_ref(),
        }
    }

    /// Canonicalizes known text to a named variant.
    /// [`StoreError::InvalidEnumValue`] when unknown text is empty, or [`StoreError::EnumValueTooLong`] when it exceeds [`MAX_ENUM_BYTES`].
    pub fn from_raw(raw: &str) -> Result<Self> {
        match raw {
            "build" => Ok(Self::Build),
            "conversation" => Ok(Self::Conversation),
            _ => Ok(Self::Other(UnknownMemberKind::parsed("kind", raw)?)),
        }
    }

    /// Decodes text read back from the store; unlike [`Self::from_raw`], it cannot fail.
    pub(crate) fn from_stored(raw: String) -> Self {
        match raw.as_str() {
            "build" => Self::Build,
            "conversation" => Self::Conversation,
            _ => Self::Other(UnknownMemberKind::stored(raw)),
        }
    }
}

/// Where a member's session came from; written once at insert and never updated by any API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberOrigin {
    Local,
    Remote,
    /// A value written by a newer binary; round-trips unchanged.
    Other(UnknownMemberOrigin),
}

impl MemberOrigin {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Other(value) => value.as_ref(),
        }
    }

    /// Canonicalizes known text to a named variant.
    /// [`StoreError::InvalidEnumValue`] when unknown text is empty, or [`StoreError::EnumValueTooLong`] when it exceeds [`MAX_ENUM_BYTES`].
    pub fn from_raw(raw: &str) -> Result<Self> {
        match raw {
            "local" => Ok(Self::Local),
            "remote" => Ok(Self::Remote),
            _ => Ok(Self::Other(UnknownMemberOrigin::parsed("origin", raw)?)),
        }
    }

    /// Decodes text read back from the store; unlike [`Self::from_raw`], it cannot fail.
    pub(crate) fn from_stored(raw: String) -> Self {
        match raw.as_str() {
            "local" => Self::Local,
            "remote" => Self::Remote,
            _ => Self::Other(UnknownMemberOrigin::stored(raw)),
        }
    }
}

/// Workspace-wide grouping preference stored in the one-row `meta` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grouping {
    State,
    Directory,
    /// A value written by a newer binary; round-trips unchanged.
    Other(UnknownGrouping),
}

impl Grouping {
    pub fn as_str(&self) -> &str {
        match self {
            Self::State => "state",
            Self::Directory => "directory",
            Self::Other(value) => value.as_ref(),
        }
    }

    /// Canonicalizes known text to a named variant.
    /// [`StoreError::InvalidEnumValue`] when unknown text is empty, or [`StoreError::EnumValueTooLong`] when it exceeds [`MAX_ENUM_BYTES`].
    pub fn from_raw(raw: &str) -> Result<Self> {
        match raw {
            "state" => Ok(Self::State),
            "directory" => Ok(Self::Directory),
            _ => Ok(Self::Other(UnknownGrouping::parsed("grouping", raw)?)),
        }
    }

    /// Decodes text read back from the store; unlike [`Self::from_raw`], it cannot fail.
    pub(crate) fn from_stored(raw: String) -> Self {
        match raw.as_str() {
            "state" => Self::State,
            "directory" => Self::Directory,
            _ => Self::Other(UnknownGrouping::stored(raw)),
        }
    }
}

/// Primary key of a workspace member.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberKey {
    pub session_id: SessionId,
    pub kind: MemberKind,
}

/// Metadata columns, exactly the set `update_member_metadata` may touch.
/// `origin` and the rank columns are not fields here, so no metadata write can clobber them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberMetadata {
    /// Required and absolute for build members; optional for other kinds.
    /// When present, at most [`MAX_CWD_BYTES`] bytes.
    pub cwd: Option<String>,
    /// Truncated to [`MAX_TITLE_BYTES`].
    pub title: Option<String>,
    /// Truncated to [`MAX_MODEL_BYTES`].
    pub model: Option<String>,
    /// Truncated to [`MAX_SUMMARY_BYTES`].
    pub last_turn_summary: Option<String>,
    pub is_worktree: bool,
    pub last_change_unix_ms: i64,
}

impl MemberMetadata {
    /// Every write path validates its metadata here, so the hottest write (the per-turn metadata sync) cannot drift from the insert path's rules.
    /// Returns a borrowed view: the per-turn sync must not pay an owned copy just to (rarely) truncate.
    /// Display text over its cap truncates instead of erroring: refusing a metadata sync over a long title would be worse than clipping it.
    pub(crate) fn validated(&self, kind: &MemberKind) -> Result<ValidatedMetadataRef<'_>> {
        if matches!(kind, MemberKind::Build) && self.cwd.is_none() {
            return Err(StoreError::CwdRequired);
        }
        if let Some(cwd) = &self.cwd {
            if !std::path::Path::new(cwd).is_absolute() {
                return Err(StoreError::CwdNotAbsolute);
            }
            if cwd.len() > MAX_CWD_BYTES {
                return Err(StoreError::CwdTooLong { max: MAX_CWD_BYTES });
            }
        }
        Ok(ValidatedMetadataRef {
            cwd: self.cwd.as_deref(),
            title: self
                .title
                .as_deref()
                .map(|text| truncate_at_char_boundary(text, MAX_TITLE_BYTES)),
            model: self
                .model
                .as_deref()
                .map(|text| truncate_at_char_boundary(text, MAX_MODEL_BYTES)),
            last_turn_summary: self
                .last_turn_summary
                .as_deref()
                .map(|text| truncate_at_char_boundary(text, MAX_SUMMARY_BYTES)),
            is_worktree: self.is_worktree,
            last_change_unix_ms: self.last_change_unix_ms,
        })
    }
}

/// Validated, capped view of a [`MemberMetadata`]; what the write paths bind into SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedMetadataRef<'a> {
    pub cwd: Option<&'a str>,
    pub title: Option<&'a str>,
    pub model: Option<&'a str>,
    pub last_turn_summary: Option<&'a str>,
    pub is_worktree: bool,
    pub last_change_unix_ms: i64,
}

fn truncate_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

/// Payload for `insert_member`.
/// `origin` is set here and only here; no later API can rewrite it.
#[derive(Debug, Clone)]
pub struct NewMember {
    pub key: MemberKey,
    pub origin: MemberOrigin,
    pub metadata: MemberMetadata,
}

/// One stored workspace member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub session_id: SessionId,
    pub kind: MemberKind,
    pub origin: MemberOrigin,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub last_turn_summary: Option<String>,
    pub is_worktree: bool,
    pub last_change_unix_ms: i64,
    /// `None` means unpinned; the rank is a gapped integer and lower sorts first.
    /// Rank values carry no meaning beyond relative order: never assume density or a fixed origin.
    pub pin_rank: Option<i64>,
    /// `None` means no manual position; otherwise behaves like `pin_rank`.
    pub order_rank: Option<i64>,
}

/// One consistent view of the whole workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub grouping: Grouping,
    /// Rows in primary-key order.
    /// Ordering policy (pins, ranks, recency) belongs to the consumer; the store returns raw deterministic rows.
    pub members: Vec<Member>,
    /// `PRAGMA data_version` captured in the same read transaction.
    /// A poller comparing against this value cannot miss a commit that landed between the snapshot and its first poll.
    pub data_version: i64,
}

/// One rank write in a `set_pin_rank` / `set_order_rank` batch.
/// `None` clears the rank (unpin / drop the manual position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankAssignment {
    pub key: MemberKey,
    pub rank: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinAssignment {
    pub key: MemberKey,
    pub pinned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum LayoutGrouping {
    State,
    Directory,
}

/// Atomic dashboard-v2 layout update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPatch {
    pub pin_assignments: Vec<PinAssignment>,
    /// Complete ordered Build subset; omitted members are cleared and `Some(vec![])` clears all.
    pub manual_order: Option<Vec<MemberKey>>,
    pub grouping: Option<LayoutGrouping>,
}

impl LayoutPatch {
    pub fn is_empty(&self) -> bool {
        self.pin_assignments.is_empty() && self.manual_order.is_none() && self.grouping.is_none()
    }
}

/// Disjoint transaction result; rejection includes a reliable post-rollback snapshot.
#[derive(Debug)]
#[must_use]
pub enum LayoutApplyOutcome {
    /// Transaction committed; this snapshot was read inside it.
    Committed(WorkspaceSnapshot),
    /// Transaction rolled back and a fresh post-rollback snapshot succeeded.
    Rejected {
        error: StoreError,
        snapshot: WorkspaceSnapshot,
    },
    /// No reliable committed snapshot is available from this attempt.
    Failed { error: StoreError },
}

/// What `insert_member` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    /// Capacity was reached: the oldest unpinned members were deleted in the same transaction (normally one; more than one heals an overfull file).
    /// Carries the evicted keys in unspecified order.
    InsertedEvicting(Vec<MemberKey>),
    /// The key already existed: metadata columns were updated; `origin` and the rank columns were left untouched.
    UpdatedExisting,
}

/// What `remove_member` did.
/// Removal is idempotent: removing an already-removed member is a success (`NotPresent`), never an error.
/// Two windows racing the same archive both succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    NotPresent,
}

/// What `rekey` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyOutcome {
    /// The row moved to the new id; its ranks traveled with it.
    Moved,
    /// A row with the target key already existed: it kept its origin and metadata.
    /// Its NULL ranks were filled from the old row, and the old row was deleted.
    MergedIntoExisting,
    /// The member already has the target id; nothing was written.
    /// This arm keeps a rekey to the member's own id out of the merge arm, whose final delete would destroy the row it just merged into.
    NoChange,
}

/// Result of the schema version gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaState {
    /// The file is at this build's schema version; all operations work.
    Current,
    /// The file was written by a newer grok.
    /// Reads remain available while its schema is read-compatible; every write returns [`StoreError::NewerSchema`].
    NewerReadOnly { user_version: u32 },
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
