// Glob re-exports keep the flat `crate::util::config::*` paths that in-crate and cross-crate callers rely on.

mod auto_mode;
mod compaction;
mod crash_handler;
mod display_refresh;
mod features;
mod flags;
mod mcp;
mod prompt_suggest;
mod request_compression;
mod system_prompt;
mod tool_approvals;
mod toolset;
mod ui;
mod version;

pub use auto_mode::*;
pub use compaction::*;
pub use crash_handler::*;
pub use display_refresh::*;
pub use features::*;
pub use flags::*;
pub use mcp::*;
pub use prompt_suggest::*;
pub(crate) use request_compression::*;
pub use system_prompt::*;
pub use tool_approvals::*;
pub use toolset::*;
pub use ui::*;
pub use version::*;

// One crate-wide mutex serializes env mutation; `permissions.rs` tests name it via this module's path
#[cfg(test)]
pub(crate) use auto_mode::AUTO_PERMISSION_MODE_ENV_LOCK;
