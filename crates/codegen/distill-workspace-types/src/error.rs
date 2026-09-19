// Modified for Distill by Samuel Fajreldines, 2026.
//! # Implementation notes
//!
//! - **`Io`**: `std::io::Error` is not `Serialize`, so `Io(#[from] std::io::Error)` cannot cross a gRPC stream.
//!   The payload is a serializable `Io { message: String, kind: IoKind }`; [`IoKind`] mirrors every stable variant of [`std::io::ErrorKind`].
//!   Conversion from `std::io::Error` happens at the workspace-crate boundary via [`WorkspaceError::from_io`].
//!
//! - **`Tool`**: `Tool(#[from] distill_tools::ToolError)` would make this crate depend on `distill-tools`, defeating the wire-types-only goal.
//!   Tool errors travel as a generic `Tool { code, message }`; the runtime crate translates its native `ToolError` into and out of this shape.
//!
//! - **`Vcs`**: a plain `Vcs(String)` payload; the runtime workspace crate translates native git/jj errors into the string.
//!   It can later become a structured `VcsErrorKind` enum without breaking the wire format (the JSON shape stays a string).
//!
//! - **`Internal`**: `Box<dyn Error>` is not `Serialize`, so the payload is `Internal(String)`.
//!   Callers with richer error types format with `format!("{err:#}")` before constructing; the error chain is lost at the wire boundary.
//!
//! - **`ProtocolMismatch.expected`**: an owned `String`, because `&'static str` cannot deserialize into a `'static` borrow.
//!   Construction sites typically pass a `&'static str` literal that is `.into()`'d at the boundary.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chunks::ChunkKind;
use crate::identity::SessionId;

/// All errors surfaced by a workspace transport. Every variant is serializable for gRPC.
/// Conversion from non-serializable runtime errors happens at the workspace-crate boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceError {
    /// Filesystem I/O failure. Mirrors `std::io::Error` shape.
    #[error("io: {message}")]
    Io {
        /// `std::io::Error::to_string()` value.
        message: String,
        /// Serializable mirror of `std::io::ErrorKind`.
        kind: IoKind,
    },

    /// Version-control (git/jj) failure.
    #[error("vcs: {0}")]
    Vcs(String),

    /// Permission denied at the workspace policy layer.
    #[error("permission denied: {reason}")]
    Permission {
        /// Human-readable reason.
        reason: String,
    },

    #[error("not found: {0}")]
    NotFound(String),

    /// Operation cancelled (caller dropped the receiver or fired the cancel token).
    #[error("cancelled")]
    Cancelled,

    #[error("deadline exceeded after {elapsed_ms}ms")]
    Timeout {
        /// Elapsed milliseconds from start to timeout.
        elapsed_ms: u64,
    },

    /// Session id was not registered with the workspace.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// A tool returned an error.
    /// The runtime crate translates its native `ToolError` into this generic shape.
    #[error("tool error [{code}]: {message}")]
    Tool {
        /// Stable, machine-readable code (e.g. `"timeout"`, `"invalid_args"`).
        code: String,
        /// Human-readable description.
        message: String,
    },

    /// Generic transport-layer failure (gRPC handshake, TLS, ...).
    #[error("transport: {0}")]
    Remote(String),

    /// The wrong chunk kind arrived on the stream.
    #[error("protocol mismatch: expected {expected}, got {got}")]
    ProtocolMismatch {
        /// Static name of the expected variant.
        expected: String,
        /// Discriminator of the chunk that actually arrived.
        got: ChunkKind,
    },

    /// The stream produced something inconsistent with the stream contract (e.g. a unary op yielded extra chunks).
    #[error("protocol violation: {0}")]
    ProtocolViolation(String),

    /// The stream closed before yielding any chunk.
    #[error("empty stream (expected at least one chunk)")]
    EmptyStream,

    /// Catch-all for unexpected internal failures.
    #[error("internal: {0}")]
    Internal(String),
}

impl WorkspaceError {
    /// Convert from a `std::io::Error`.
    /// Used at the workspace-crate boundary; this crate does not implement `From<io::Error>` because `io::Error` is not serializable.
    pub fn from_io(err: std::io::Error) -> Self {
        Self::Io {
            message: err.to_string(),
            kind: IoKind::from(err.kind()),
        }
    }

    /// Whether the operation is safe to retry.
    /// Only [`Self::Timeout`], [`Self::Remote`], and transient [`Self::Io`]; domain errors are not.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout { .. } | Self::Remote(_) => true,
            Self::Io { kind, .. } => kind.is_transient(),
            _ => false,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// Serializable mirror of [`std::io::ErrorKind`] (stable variants as of Rust 1.83+).
/// Enumerated variants convert losslessly; future-stabilized ones collapse to [`IoKind::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoKind {
    ConnectionRefused,
    ConnectionReset,
    HostUnreachable,
    NetworkUnreachable,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    NetworkDown,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    ReadOnlyFilesystem,
    StaleNetworkFileHandle,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    StorageFull,
    NotSeekable,
    QuotaExceeded,
    FileTooLarge,
    ResourceBusy,
    ExecutableFileBusy,
    Deadlock,
    CrossesDevices,
    TooManyLinks,
    InvalidFilename,
    ArgumentListTooLong,
    Interrupted,
    UnexpectedEof,
    Unsupported,
    OutOfMemory,
    NotFound,
    PermissionDenied,
    /// Other / unrecognized (catches future-stabilized variants).
    Other,
}

impl IoKind {
    /// Whether the I/O kind is transient (the same operation may succeed if retried).
    /// Used by [`WorkspaceError::is_retryable`].
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::BrokenPipe
                | Self::ConnectionReset
                | Self::ConnectionAborted
                | Self::ConnectionRefused
                | Self::TimedOut
                | Self::Interrupted
                | Self::WouldBlock
                | Self::HostUnreachable
                | Self::NetworkUnreachable
                | Self::NetworkDown
                | Self::ResourceBusy
                | Self::Deadlock
        )
    }
}

impl From<std::io::ErrorKind> for IoKind {
    fn from(kind: std::io::ErrorKind) -> Self {
        use std::io::ErrorKind as K;
        match kind {
            K::NotFound => Self::NotFound,
            K::PermissionDenied => Self::PermissionDenied,
            K::ConnectionRefused => Self::ConnectionRefused,
            K::ConnectionReset => Self::ConnectionReset,
            K::HostUnreachable => Self::HostUnreachable,
            K::NetworkUnreachable => Self::NetworkUnreachable,
            K::ConnectionAborted => Self::ConnectionAborted,
            K::NotConnected => Self::NotConnected,
            K::AddrInUse => Self::AddrInUse,
            K::AddrNotAvailable => Self::AddrNotAvailable,
            K::NetworkDown => Self::NetworkDown,
            K::BrokenPipe => Self::BrokenPipe,
            K::AlreadyExists => Self::AlreadyExists,
            K::WouldBlock => Self::WouldBlock,
            K::NotADirectory => Self::NotADirectory,
            K::IsADirectory => Self::IsADirectory,
            K::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            K::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            K::StaleNetworkFileHandle => Self::StaleNetworkFileHandle,
            K::InvalidInput => Self::InvalidInput,
            K::InvalidData => Self::InvalidData,
            K::TimedOut => Self::TimedOut,
            K::WriteZero => Self::WriteZero,
            K::StorageFull => Self::StorageFull,
            K::NotSeekable => Self::NotSeekable,
            K::QuotaExceeded => Self::QuotaExceeded,
            K::FileTooLarge => Self::FileTooLarge,
            K::ResourceBusy => Self::ResourceBusy,
            K::ExecutableFileBusy => Self::ExecutableFileBusy,
            K::Deadlock => Self::Deadlock,
            K::CrossesDevices => Self::CrossesDevices,
            K::TooManyLinks => Self::TooManyLinks,
            K::InvalidFilename => Self::InvalidFilename,
            K::ArgumentListTooLong => Self::ArgumentListTooLong,
            K::Interrupted => Self::Interrupted,
            K::UnexpectedEof => Self::UnexpectedEof,
            K::Unsupported => Self::Unsupported,
            K::OutOfMemory => Self::OutOfMemory,
            // Future-stabilized variants (and the historical `Other` / `Uncategorized`) bucket here
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_mismatch_uses_chunk_kind_display_not_debug() {
        let err = WorkspaceError::ProtocolMismatch {
            expected: "GitStatus".into(),
            got: ChunkKind::Ack,
        };
        // Display, not Debug: a `{got:?}` format would still pass today because they coincide.
        assert_eq!(
            err.to_string(),
            "protocol mismatch: expected GitStatus, got Ack"
        );
    }

    #[test]
    fn is_retryable_only_for_transient_io_remote_timeout() {
        assert!(WorkspaceError::Timeout { elapsed_ms: 1 }.is_retryable());
        assert!(WorkspaceError::Remote("x".into()).is_retryable());
        for kind in [
            IoKind::BrokenPipe,
            IoKind::ConnectionReset,
            IoKind::ConnectionAborted,
            IoKind::ConnectionRefused,
            IoKind::TimedOut,
            IoKind::Interrupted,
            IoKind::WouldBlock,
            IoKind::HostUnreachable,
            IoKind::NetworkUnreachable,
            IoKind::NetworkDown,
            IoKind::ResourceBusy,
            IoKind::Deadlock,
        ] {
            assert!(
                WorkspaceError::Io {
                    message: "x".into(),
                    kind
                }
                .is_retryable(),
                "expected {kind:?} to be retryable"
            );
        }
        for kind in [
            IoKind::NotFound,
            IoKind::PermissionDenied,
            IoKind::InvalidInput,
            IoKind::InvalidData,
            IoKind::AlreadyExists,
            IoKind::Unsupported,
            IoKind::WriteZero,
            IoKind::IsADirectory,
            IoKind::NotADirectory,
            IoKind::ReadOnlyFilesystem,
            IoKind::StorageFull,
            IoKind::FileTooLarge,
            IoKind::Other,
        ] {
            assert!(
                !WorkspaceError::Io {
                    message: "x".into(),
                    kind
                }
                .is_retryable(),
                "expected {kind:?} to be non-retryable"
            );
        }
        assert!(!WorkspaceError::Cancelled.is_retryable());
        assert!(!WorkspaceError::Permission { reason: "x".into() }.is_retryable());
        assert!(!WorkspaceError::EmptyStream.is_retryable());
    }

    #[test]
    fn from_io_preserves_kind_and_message() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing.txt");
        let err = WorkspaceError::from_io(io);
        match err {
            WorkspaceError::Io { kind, message } => {
                assert_eq!(kind, IoKind::NotFound);
                assert!(message.contains("missing.txt"), "got {message}");
            }
            other => panic!("expected Io variant, got {other:?}"),
        }
    }

    #[test]
    fn io_kind_from_round_trips_for_every_std_kind() {
        use std::io::ErrorKind as K;
        let cases: &[(K, IoKind)] = &[
            (K::NotFound, IoKind::NotFound),
            (K::PermissionDenied, IoKind::PermissionDenied),
            (K::ConnectionRefused, IoKind::ConnectionRefused),
            (K::ConnectionReset, IoKind::ConnectionReset),
            (K::HostUnreachable, IoKind::HostUnreachable),
            (K::NetworkUnreachable, IoKind::NetworkUnreachable),
            (K::ConnectionAborted, IoKind::ConnectionAborted),
            (K::NotConnected, IoKind::NotConnected),
            (K::AddrInUse, IoKind::AddrInUse),
            (K::AddrNotAvailable, IoKind::AddrNotAvailable),
            (K::NetworkDown, IoKind::NetworkDown),
            (K::BrokenPipe, IoKind::BrokenPipe),
            (K::AlreadyExists, IoKind::AlreadyExists),
            (K::WouldBlock, IoKind::WouldBlock),
            (K::NotADirectory, IoKind::NotADirectory),
            (K::IsADirectory, IoKind::IsADirectory),
            (K::DirectoryNotEmpty, IoKind::DirectoryNotEmpty),
            (K::ReadOnlyFilesystem, IoKind::ReadOnlyFilesystem),
            (K::StaleNetworkFileHandle, IoKind::StaleNetworkFileHandle),
            (K::InvalidInput, IoKind::InvalidInput),
            (K::InvalidData, IoKind::InvalidData),
            (K::TimedOut, IoKind::TimedOut),
            (K::WriteZero, IoKind::WriteZero),
            (K::StorageFull, IoKind::StorageFull),
            (K::NotSeekable, IoKind::NotSeekable),
            (K::QuotaExceeded, IoKind::QuotaExceeded),
            (K::FileTooLarge, IoKind::FileTooLarge),
            (K::ResourceBusy, IoKind::ResourceBusy),
            (K::ExecutableFileBusy, IoKind::ExecutableFileBusy),
            (K::Deadlock, IoKind::Deadlock),
            (K::CrossesDevices, IoKind::CrossesDevices),
            (K::TooManyLinks, IoKind::TooManyLinks),
            (K::InvalidFilename, IoKind::InvalidFilename),
            (K::ArgumentListTooLong, IoKind::ArgumentListTooLong),
            (K::Interrupted, IoKind::Interrupted),
            (K::UnexpectedEof, IoKind::UnexpectedEof),
            (K::Unsupported, IoKind::Unsupported),
            (K::OutOfMemory, IoKind::OutOfMemory),
        ];
        for &(k, expected) in cases {
            assert_eq!(IoKind::from(k), expected, "mismatch for {k:?}");
        }
    }
}
