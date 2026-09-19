//! Typed failures for the workspace store, and the classification helpers that map SQLite failures onto them.

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Every typed failure the store API can return.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The file was written by a newer grok (`user_version` above what this build supports).
    /// The handle stays open read-only; every write returns this.
    #[error("workspace store is schema v{found}; this build supports v{supported} (read-only)")]
    NewerSchema { found: u32, supported: u32 },

    /// `insert_member` hit capacity with zero unpinned members; the only capacity case the user ever sees.
    #[error("workspace is full ({capacity} members) and every member is pinned")]
    AllPinned { capacity: usize },

    /// Returned where a missing row is a real caller error (`update_member_metadata`, the rank setters, `rekey`).
    /// Never returned by `remove_member`, which is idempotent.
    #[error("no workspace member {session_id} ({kind})")]
    MemberNotFound { session_id: String, kind: String },

    #[error("invalid workspace layout patch: {reason}")]
    InvalidLayoutPatch { reason: &'static str },

    /// Rejection from [`crate::SessionId::new`].
    #[error("invalid session id: {reason}")]
    InvalidSessionId { reason: &'static str },

    #[error("cwd is required for build members")]
    CwdRequired,

    #[error("cwd must be an absolute path")]
    CwdNotAbsolute,

    #[error("cwd exceeds {max} bytes")]
    CwdTooLong { max: usize },

    /// A caller-supplied unknown enum value that is structurally invalid.
    /// Values read back from the store are exempt for forward compatibility.
    #[error("invalid unknown {column} value: {reason}")]
    InvalidEnumValue {
        column: &'static str,
        reason: &'static str,
    },

    /// A caller-supplied unknown enum value over the byte cap.
    /// Rejected, never truncated: a clipped `kind` would address a different primary key.
    /// A clipped origin or grouping would break byte-identical passthrough.
    #[error("unknown {column} value exceeds {max} bytes")]
    EnumValueTooLong { column: &'static str, max: usize },

    /// The busy timeout elapsed while another connection held the write lock.
    /// `waited_ms` is measured by the store around the failing operation; SQLite's busy handler reports nothing.
    #[error("workspace store busy after {waited_ms}ms")]
    Busy { waited_ms: u64 },

    /// The file is corrupt or is not a SQLite database.
    /// The store never deletes, truncates, or quarantines it: the user's workspace stays on disk for manual recovery.
    #[error("workspace store file is unusable: {source}")]
    Unusable {
        #[source]
        source: rusqlite::Error,
    },

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn is_busy_code(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn is_unusable_db_error(error: &rusqlite::Error) -> bool {
    match error {
        rusqlite::Error::SqliteFailure(f, message) => {
            matches!(
                f.code,
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
            ) || message.as_deref().is_some_and(|message| {
                let message = message.to_ascii_lowercase();
                message.contains("disk image is malformed")
                    || message.contains("malformed database schema")
                    || message.contains("is not a database")
            })
        }
        _ => false,
    }
}

pub(crate) fn classify_unusable(error: rusqlite::Error) -> StoreError {
    if is_unusable_db_error(&error) {
        StoreError::Unusable { source: error }
    } else {
        StoreError::Sqlite(error)
    }
}

/// Single classification point so every operation maps an exhausted busy budget to the same typed error.
/// The elapsed time is measured here because SQLite's busy handler reports nothing.
pub(crate) fn classify_busy(
    error: StoreError,
    op: &'static str,
    started: std::time::Instant,
) -> StoreError {
    match error {
        StoreError::Sqlite(e) if is_busy_code(&e) => {
            let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::warn!(op, waited_ms, "workspace store busy budget exhausted");
            StoreError::Busy { waited_ms }
        }
        other => other,
    }
}

/// Open-path classification: maps SQLite corruption, preserves already typed store errors, then applies busy classification.
/// The store never deletes or recreates an unusable file.
pub(crate) fn classify_open_error(
    error: StoreError,
    started: std::time::Instant,
    path: &std::path::Path,
) -> StoreError {
    let error = match error {
        StoreError::Sqlite(error) => classify_unusable(error),
        other => other,
    };
    if matches!(error, StoreError::Unusable { .. }) {
        tracing::error!(path = %path.display(), error = %error, "workspace store file is unusable");
    }
    classify_busy(error, "open", started)
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
