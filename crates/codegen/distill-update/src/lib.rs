#![deny(clippy::indexing_slicing)]

pub mod auto_update;
mod cleanup_downloads;
pub mod version;
mod version_policy;

pub use auto_update::UpdateStatus;
pub use version::{UpdateConfig, channel_label, channel_name, write_version_cache};
pub use version_policy::enforce_version_policy_or_exit;
