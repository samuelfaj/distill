// Modified for Distill by Samuel Fajreldines, 2026.
//! Auth traits shared between `distill-file-utils` (the holder) and `distill-shell` (the implementer).
//! Keeps shell types out of data-collector's import graph while still letting refresh-aware token resolution drive HTTP requests.

#![deny(clippy::indexing_slicing)]

pub mod auth_provider;
pub mod bearer_fragment;
#[cfg(feature = "middleware")]
pub mod retry_middleware;
pub mod visibility;

pub use auth_provider::{AuthCredentialProvider, CredentialSnapshot, StaticAuthCredentialProvider};
pub use bearer_fragment::{BEARER_SUFFIX_LEN, bearer_suffix};
#[cfg(feature = "middleware")]
pub use retry_middleware::{AuthRetryMiddleware, StampedBearerSuffix, execute_with_stamp};
pub use visibility::HttpAuth;
