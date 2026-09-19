// Modified for Distill by Samuel Fajreldines, 2026.
//! Which host runs a workspace server, and what its credential lets the server do.
//!
//! A sandbox's credential reaches the Grok API; every other host's only serves the hub. The
//! catalog table is shared with the sandbox, so the tools that would call the API with the
//! server's own credential are cut per host here and nowhere else.

use distill_computer_hub_sdk::SharedAuthProvider;
use distill_tools::registry::types::ToolServerConfig;

use crate::session::tool_config::WorkspaceSessionContextFactory;

/// The host running a workspace server. The sandbox binary announces [`Self::as_wire_str`] as the
/// registration's `host_kind`; `grok-workspaced` announces its own kind and takes the default here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceHostKind {
    /// A hosted sandbox.
    Sandbox,
    /// A daemon on a user's or remote machine, whoever started it (Grok Desktop, the `grok` CLI,
    /// or by hand). The fail-closed default for a caller that names no host.
    #[default]
    Daemon,
}

impl WorkspaceHostKind {
    /// The `host_kind` string for registration metadata (`distill_tool_protocol::HOST_KIND_*`).
    pub fn as_wire_str(self) -> &'static str {
        match self {
            WorkspaceHostKind::Sandbox => distill_tool_protocol::HOST_KIND_SANDBOX,
            WorkspaceHostKind::Daemon => distill_tool_protocol::HOST_KIND_DAEMON,
        }
    }

    /// Whether the host's credential only serves the hub: every host but the sandbox. Such a host
    /// advertises no tool that calls the API with that credential, hands the credential to none,
    /// cannot upload, and refuses a bind that names no toolset (`missing_tool_config`) rather than
    /// widen to its catalog: the binder decides its toolset.
    pub fn is_hub_only(self) -> bool {
        match self {
            WorkspaceHostKind::Sandbox => false,
            WorkspaceHostKind::Daemon => true,
        }
    }

    /// Whether the server streams `FsChanged` for its root. A daemon's folder is edited outside
    /// the agent (the user's editor, git) and a bound client wants to hear it; a sandbox's root
    /// changes only through the agent's own tools, and arming an OS watcher per container would
    /// cost a tree walk and a frame per write that no harness reads.
    pub fn streams_fs_changes(self) -> bool {
        match self {
            WorkspaceHostKind::Sandbox => false,
            WorkspaceHostKind::Daemon => true,
        }
    }

    /// The catalog a server on this host advertises to the hub, and serves when a bind names no
    /// toolset and the host lets it: the sandbox's, minus the API-backed tools on a hub-only host.
    pub fn default_toolset(self) -> ToolServerConfig {
        let mut catalog = distill_agent::workspace_distill_toolset();
        if self.is_hub_only() {
            let api_backed = distill_agent::api_backed_tool_ids();
            catalog.tools.retain(|tool| !api_backed.contains(&tool.id));
        }
        catalog
    }

    /// The session-context factory for this host: only one whose credential reaches the API hands
    /// `auth` to the gen and search tool configs and to the session `Resources`.
    pub(crate) fn session_context_factory(
        self,
        auth: SharedAuthProvider,
        api_base_url: String,
    ) -> WorkspaceSessionContextFactory {
        if self.is_hub_only() {
            WorkspaceSessionContextFactory::hub_only()
        } else {
            WorkspaceSessionContextFactory::with_auth(auth, api_base_url)
        }
    }
}

#[cfg(test)]
#[path = "host_kind_tests.rs"]
mod tests;
