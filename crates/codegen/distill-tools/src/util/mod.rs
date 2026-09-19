// Modified for Distill by Samuel Fajreldines, 2026.
pub mod base64_images;
pub mod binary;
pub mod command_display;
pub mod env;
pub mod fs;
pub mod git_detect;
pub mod distill_home;
pub mod hash;
pub mod image_compress;
pub use distill_image as image_validate;
pub mod mcp_structured_content;
pub mod mcp_truncate;
pub mod path_suggestions;
pub(crate) mod query_tools;
pub mod remap;
pub mod serde_base64;
pub(crate) mod shared_http;
pub mod shell_env_policy;
pub mod spawn;
pub mod truncate;
pub mod unicode_confusables;
#[cfg(any(bundle_rg, bundle_fd, bundle_bfs, bundle_ugrep, test))]
pub(crate) mod vendor;

pub use crate::implementations::distill::grep::ripgrep::rg_path;
pub use command_display::strip_redundant_session_cd;
#[cfg(unix)]
pub use env::detach_from_tty;
pub use env::substitute_plugin_tokens;
pub use env::{GROK_AGENT_ENV, GROK_AGENT_ENV_VALUE, apply_grok_agent_marker, pager_env};
pub use fs::{UnicodePathMatch, canonicalize_with_timeout, try_resolve_unicode_filename};
pub use distill_home::{grok_application, distill_home};
pub use path_suggestions::format_not_found_error;
pub use remap::{remap_json_keys, remap_schema_properties, reverse_map};
pub use shell_env_policy::{
    EnvironmentVariablePattern, ShellEnvironmentPolicy, ShellEnvironmentPolicyInherit,
    apply_shell_environment_policy,
};
pub use spawn::{
    ProcessGroup, ProcessScope, detach_command, detach_search_command, global_process_scope,
    new_process_group, reap_killed_search_child,
};
pub use truncate::{
    DEFAULT_SOFT_WRAP_WIDTH, ceil_char_boundary, estimate_tokens, floor_char_boundary,
    format_bytes, soft_wrap_line, soft_wrap_lines, truncate_line, truncate_str,
    truncate_str_with_marker,
};
pub use distill_tty_utils::detach_std_command;
