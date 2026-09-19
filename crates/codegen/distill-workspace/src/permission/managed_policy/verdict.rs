//! The single MCP policy verdict API: every shell consumer resolves verdicts here instead of
//! assembling its own deny/allow/pin reasons.

use std::path::{Path, PathBuf};

use super::ManagedSettings;
use super::mcp::PolicySubjectOrigin;

/// Policy verdict for one MCP server definition.
#[derive(Debug, Clone)]
pub enum McpVerdict {
    Allowed,
    Blocked(McpBlockReason),
}

/// Why policy blocks an MCP server, attributed to the blocking source. The
/// `Display` strings (full policy path) are doctor/JSON/log payloads;
/// [`Self::user_facing_reason`] is the refusal form — do not reword either.
#[derive(Debug, Clone)]
pub enum McpBlockReason {
    /// Matches a `deniedMcpServers` entry.
    Deny { source: PathBuf },
    /// Missing from `allowedMcpServers` (or a managed-only lockdown's grant list).
    NotGranted { source: PathBuf },
    /// The source blocks everything it binds (see `McpServerAllowlist::is_lockdown`),
    /// so no allow entry could have granted the server.
    Lockdown { source: PathBuf },
    /// Project-declared and not allowlisted under an
    /// `enableAllProjectMcpServers = false` pin.
    ProjectPin { source: PathBuf },
}

impl McpBlockReason {
    /// The policy source the block is attributed to.
    pub fn source(&self) -> &Path {
        match self {
            Self::Deny { source }
            | Self::NotGranted { source }
            | Self::Lockdown { source }
            | Self::ProjectPin { source } => source,
        }
    }

    /// The blocking policy file by name only, for user-facing refusals.
    pub fn user_facing_source(&self) -> std::borrow::Cow<'_, str> {
        user_facing_policy_source(self.source())
    }

    /// [`Display`](std::fmt::Display) with the blocking policy file reduced to its name — the
    /// user-facing refusal form; doctor, `--json`, and tracing keep the full path.
    pub fn user_facing_reason(&self) -> String {
        self.reason_with(self.user_facing_source())
    }

    /// The matched rule without its source, for renderers that print the source themselves.
    pub fn rule(&self) -> &'static str {
        match self {
            Self::Deny { .. } => "matches deniedMcpServers",
            Self::NotGranted { .. } => "not in allowedMcpServers",
            Self::Lockdown { .. } => "locked down by policy",
            Self::ProjectPin { .. } => "project MCP disabled by enableAllProjectMcpServers = false",
        }
    }

    fn reason_with(&self, source: impl std::fmt::Display) -> String {
        format!("{} ({source})", self.rule())
    }
}

impl std::fmt::Display for McpBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason_with(self.source().display()))
    }
}

/// The policy file's name for user-facing refusals; falls back to the full
/// path when it has no file name (empty/unknown source).
pub(super) fn user_facing_policy_source(path: &Path) -> std::borrow::Cow<'_, str> {
    path.file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| path.to_string_lossy())
}

/// What defined the server, as policy sees it.
#[derive(Debug, Clone, Copy)]
pub struct McpSubject {
    pub origin: PolicySubjectOrigin,
    /// Declared by a project source (drives the project-MCP pin).
    pub project_scoped: bool,
}

impl ManagedSettings {
    /// Project-pin leg of [`Self::mcp_verdict`] in isolation: blocks a project-scoped
    /// server lacking an ownership-satisfying grant (the session merge applies it first).
    pub fn mcp_project_pin_block(
        &self,
        server: &agent_client_protocol::McpServer,
        subject: McpSubject,
    ) -> Option<McpBlockReason> {
        let super::PolicyPin::Disabled { source, ownership } = &self.project_mcp else {
            return None;
        };
        if !subject.project_scoped || self.mcp_allowlist.grants_exception(server, *ownership) {
            return None;
        }
        Some(McpBlockReason::ProjectPin {
            source: source.clone(),
        })
    }

    /// Policy verdict for `server`: binding deny, then lockdown or missing allow, then the project-MCP pin; otherwise allowed.
    /// Session merge applies the pin first; this order is for surfaces, which name the most specific rule.
    pub fn mcp_verdict(
        &self,
        server: &agent_client_protocol::McpServer,
        subject: McpSubject,
    ) -> McpVerdict {
        let policy = &self.mcp_allowlist;
        if let Some(denying) = policy.denying_source(server, subject.origin) {
            return McpVerdict::Blocked(McpBlockReason::Deny {
                source: denying.source_path.clone().unwrap_or_default(),
            });
        }
        if let Some(blocking) = policy.blocking_allow_source(server, subject.origin) {
            let source = blocking.source_path.clone().unwrap_or_default();
            return McpVerdict::Blocked(if blocking.is_lockdown() {
                McpBlockReason::Lockdown { source }
            } else {
                McpBlockReason::NotGranted { source }
            });
        }
        if let Some(reason) = self.mcp_project_pin_block(server, subject) {
            return McpVerdict::Blocked(reason);
        }
        McpVerdict::Allowed
    }
}
