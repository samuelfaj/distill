//! `[cursor_worker]` section from config.toml.
//! Always parsed so a build without worker support still accepts the table;
//! only a leader compiled with it acts on the values. Re-exported from `agent::config`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CursorWorkerConfig {
    /// Register the worker when the leader starts.
    pub auto_start: bool,
    /// Display name for this worker; the hostname when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absolute repository directories offered to claimed agents; the first is the registered
    /// working directory.
    pub worker_dirs: Vec<String>,
    /// Cap on concurrently claimed agents; `None` is uncapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agents: Option<u32>,
    /// Also register the any-repo door, which clones whatever repository a claimed agent brings.
    /// Opt-in: only `Some(true)` plans it, alone or beside the bound door. `None` and
    /// `Some(false)` never plan it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any_repo: Option<bool>,
    /// Hub base URL for the worker session; derived from the leader's hub URL when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_url: Option<String>,
    /// Rewrite worktree and snapshot paths in shell output back to the registered directory.
    #[serde(default = "default_rewrite_shell_output")]
    pub rewrite_shell_output: bool,
}

fn default_rewrite_shell_output() -> bool {
    true
}

impl Default for CursorWorkerConfig {
    fn default() -> Self {
        Self {
            auto_start: false,
            name: None,
            worker_dirs: Vec::new(),
            max_agents: None,
            any_repo: None,
            hub_url: None,
            rewrite_shell_output: true,
        }
    }
}
