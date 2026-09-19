//! MCP server policy: allow/deny entries, per-source allowlists, and the
//! cross-source [`McpServerPolicy`] (any deny wins, every restricted source
//! must allow, managed-only requires a positive grant).

use std::path::Path;

use super::layer::{PolicyLayerOwnership, PolicySourceAuthority};
use super::url_match::{AllowUrlMatcher, DenyUrlMatcher, argv_matches, mcp_server_name};

/// What defined the server or marketplace a policy is applied to.
/// `GrokNative` (user/system config, plugins, admin pins) is exempt from advisory sources; everything else is `Foreign` and subject to every source.
/// Ambiguity classifies as `Foreign` (fail closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySubjectOrigin {
    GrokNative,
    Foreign,
}

#[derive(Debug, Clone)]
pub enum AllowedMcpServer {
    Http {
        url_pattern: String,
    },
    Stdio {
        command: String,
    },
    /// Claude `serverCommand`: exact argv match on `[command, args...]`.
    StdioArgv {
        argv: Vec<String>,
    },
    /// Match by config name (any transport); see [`mcp_name_matches`].
    Name {
        name: String,
    },
}

/// MCP policy from ONE managed source; deny beats allow.
#[derive(Debug, Clone)]
pub struct McpServerAllowlist {
    /// Private: `new` (the only constructor) compiles each entry's matcher.
    entries: Vec<CompiledEntry<AllowUrlMatcher>>,
    deny_entries: Vec<CompiledEntry<DenyUrlMatcher>>,
    /// `allowManagedMcpServersOnly`: a positive allow-entry match is required.
    managed_only: bool,
    /// Present-but-empty allow list (vendor managed-settings semantics) or malformed key:
    /// every server this source binds is blocked.
    lockdown: bool,
    /// Who can write the layer this source came from (see
    /// [`PolicyLayerOwnership`] for the grant rules).
    ownership: PolicyLayerOwnership,
    pub source_path: Option<std::path::PathBuf>,
    /// Whether this source's restrictions bind grok-native servers.
    authority: PolicySourceAuthority,
}

/// One policy entry with its URL matcher compiled at construction, stored
/// together so the compiled form can't drift from the entry list (a
/// pattern-keyed side map would fail open on a miss).
#[derive(Debug, Clone)]
struct CompiledEntry<M> {
    entry: AllowedMcpServer,
    /// `Some` iff `entry` is [`AllowedMcpServer::Http`].
    url_matcher: Option<M>,
}

fn compile_entries<M>(
    entries: Vec<AllowedMcpServer>,
    compile: fn(&str) -> M,
) -> Vec<CompiledEntry<M>> {
    entries
        .into_iter()
        .map(|entry| {
            let url_matcher = match &entry {
                AllowedMcpServer::Http { url_pattern } => Some(compile(url_pattern)),
                _ => None,
            };
            CompiledEntry { entry, url_matcher }
        })
        .collect()
}

/// The runtime URL an `Http` policy entry matches against (`Http`/`Sse`
/// transports; anything else has no URL to grant or deny by).
fn server_url(server: &agent_client_protocol::McpServer) -> Option<&str> {
    match server {
        agent_client_protocol::McpServer::Http(agent_client_protocol::McpServerHttp {
            url,
            ..
        })
        | agent_client_protocol::McpServer::Sse(agent_client_protocol::McpServerSse {
            url, ..
        }) => Some(url),
        _ => None,
    }
}

/// Entry-vs-server match on the NON-URL dimensions (name: any transport;
/// command/argv: Stdio). Callers route `Http` entries through the
/// precompiled matchers first, so `Http` here deliberately yields `false`.
fn mcp_entry_matches(entry: &AllowedMcpServer, server: &agent_client_protocol::McpServer) -> bool {
    match (entry, server) {
        (AllowedMcpServer::Name { name }, _) => mcp_name_matches(name, mcp_server_name(server)),
        (
            AllowedMcpServer::Stdio { command },
            agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio {
                command: server_command,
                ..
            }),
        ) => *command == server_command.to_string_lossy(),
        (
            AllowedMcpServer::StdioArgv { argv },
            agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio {
                command: server_command,
                args,
                ..
            }),
        ) => argv_matches(argv, server_command, args),
        _ => false,
    }
}

/// `McpServer` is `#[non_exhaustive]`: Http / Sse / Stdio are the only
/// transports this build can inspect. A new ACP variant must fail closed.
pub(super) fn mcp_transport_known(server: &agent_client_protocol::McpServer) -> bool {
    matches!(
        server,
        agent_client_protocol::McpServer::Http(_)
            | agent_client_protocol::McpServer::Sse(_)
            | agent_client_protocol::McpServer::Stdio(_)
    )
}

/// Whether an entry restricts `server`'s transport at all.
/// A transport this build can't inspect is restricted by every entry so a new
/// ACP variant cannot slip a lockdown.
fn mcp_entry_restricts(
    entry: &AllowedMcpServer,
    server: &agent_client_protocol::McpServer,
) -> bool {
    if !mcp_transport_known(server) {
        return true;
    }
    match entry {
        AllowedMcpServer::Name { .. } => true,
        AllowedMcpServer::Http { .. } => matches!(
            server,
            agent_client_protocol::McpServer::Http(_) | agent_client_protocol::McpServer::Sse(_)
        ),
        AllowedMcpServer::Stdio { .. } | AllowedMcpServer::StdioArgv { .. } => {
            matches!(server, agent_client_protocol::McpServer::Stdio(_))
        }
    }
}

impl McpServerAllowlist {
    /// Public so tests can build policies without a file on disk. Ownership starts
    /// `User`: its lockdown accepts any grant; its entries lift only user-owned ones.
    pub fn new(
        entries: Vec<AllowedMcpServer>,
        deny_entries: Vec<AllowedMcpServer>,
        source_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            entries: compile_entries(entries, AllowUrlMatcher::new),
            deny_entries: compile_entries(deny_entries, DenyUrlMatcher::new),
            managed_only: false,
            lockdown: false,
            ownership: PolicyLayerOwnership::User,
            source_path,
            authority: PolicySourceAuthority::default(),
        }
    }

    /// `allowManagedMcpServersOnly`: only positively allowlisted servers run.
    pub fn with_managed_only(mut self) -> Self {
        self.managed_only = true;
        self
    }

    /// Mark this source a full lockdown: nothing it binds runs.
    pub fn with_lockdown(mut self) -> Self {
        self.lockdown = true;
        self
    }

    /// Set how this source binds (see [`PolicySourceAuthority`]).
    pub fn with_authority(mut self, authority: PolicySourceAuthority) -> Self {
        self.authority = authority;
        self
    }

    /// Set who can write this source's layer (see [`PolicyLayerOwnership`]).
    pub fn with_ownership(mut self, ownership: PolicyLayerOwnership) -> Self {
        self.ownership = ownership;
        self
    }

    /// Whether this source's restrictions apply to a subject of `origin`:
    /// native sources bind everything, advisory sources only foreign subjects.
    fn binds(&self, origin: PolicySubjectOrigin) -> bool {
        self.authority == PolicySourceAuthority::Native || origin == PolicySubjectOrigin::Foreign
    }

    pub fn managed_only(&self) -> bool {
        self.managed_only
    }

    /// Full lockdown; reporting surfaces must not render it unrestricted just
    /// because it has zero entries.
    pub fn is_lockdown(&self) -> bool {
        self.lockdown
    }

    /// How this source binds (see [`PolicySourceAuthority`]).
    pub fn authority(&self) -> PolicySourceAuthority {
        self.authority
    }

    pub fn is_restricted(&self) -> bool {
        self.lockdown
            || self.managed_only
            || !self.entries.is_empty()
            || !self.deny_entries.is_empty()
    }

    /// Per-dimension allow check without the deny or managed-only checks (those
    /// are policy-level). A locked-down source allows nothing; no grant punches through.
    fn allows_ignoring_managed_only(&self, server: &agent_client_protocol::McpServer) -> bool {
        if self.lockdown {
            return false;
        }
        if !self.is_restricted() {
            return true;
        }
        let restricted = self
            .entries
            .iter()
            .any(|compiled| mcp_entry_restricts(&compiled.entry, server));
        !restricted || self.matches_allow_entry(server)
    }

    /// Allow entries (display/diagnostics; matching goes through the
    /// precompiled matchers).
    pub fn entries(&self) -> impl Iterator<Item = &AllowedMcpServer> {
        self.entries.iter().map(|compiled| &compiled.entry)
    }

    /// Deny entries (display/diagnostics).
    pub fn deny_entries(&self) -> impl Iterator<Item = &AllowedMcpServer> {
        self.deny_entries.iter().map(|compiled| &compiled.entry)
    }

    /// Positive grant: matches at least one allow entry (an entry-less
    /// dimension grants nothing) — the lockdown / project-exception check.
    /// URL entries hit the matcher compiled with them in `new`.
    pub(super) fn matches_allow_entry(&self, server: &agent_client_protocol::McpServer) -> bool {
        self.entries
            .iter()
            .any(|compiled| match &compiled.url_matcher {
                Some(matcher) => server_url(server).is_some_and(|url| matcher.matches(url)),
                None => mcp_entry_matches(&compiled.entry, server),
            })
    }

    /// Explicit `deniedMcpServers` match (vs merely missing from the
    /// allowlist); URL denies are host-normalized via [`DenyUrlMatcher`].
    /// Unrecognized ACP transports fail closed when this source has deny entries.
    pub fn is_server_denied(&self, server: &agent_client_protocol::McpServer) -> bool {
        self.is_server_denied_known(server, mcp_transport_known(server))
    }

    pub(super) fn is_server_denied_known(
        &self,
        server: &agent_client_protocol::McpServer,
        known: bool,
    ) -> bool {
        if !known {
            return !self.deny_entries.is_empty();
        }
        self.deny_entries
            .iter()
            .any(|compiled| match &compiled.url_matcher {
                Some(matcher) => server_url(server).is_some_and(|url| matcher.matches(url)),
                None => mcp_entry_matches(&compiled.entry, server),
            })
    }
}

/// Namespace prefix for legacy injected MCP server names (`grok_com_*`).
/// Policy matching still uses this spelling.
pub(super) const MANAGED_MCP_PREFIX: &str = "grok_com_";

/// Max `char` length of a managed runtime name (`grok_com_` + normalized display
/// name), sized to the 64-char tool-name budget. Shared with `mcp_name_matches`
/// so a long policy `serverName` still matches its truncated runtime name.
pub(super) const MANAGED_MCP_NAME_MAX_CHARS: usize = 39;

/// Normalize a bare MCP display name to its runtime spelling (lowercase, spaces
/// → `_`). Shared with `mcp_name_matches` so the policy and runtime sides never
/// drift.
pub(super) fn normalize_managed_name(bare: &str) -> String {
    bare.to_lowercase().replace(' ', "_")
}

/// Match a policy `serverName` against a runtime server name: both sides
/// reduce to one key (strip `grok_com_`, [`normalize_managed_name`]) and
/// compare by exact equality — never substring; an empty key never matches.
pub(super) fn mcp_name_matches(pattern: &str, name: &str) -> bool {
    fn key(s: &str) -> String {
        normalize_managed_name(s.strip_prefix(MANAGED_MCP_PREFIX).unwrap_or(s))
    }
    // Mirror the legacy injected-name truncation: `grok_com_*` runtime
    // names were capped at MANAGED_MCP_NAME_MAX_CHARS total (prefix-inclusive
    // on the bare part).
    fn truncate(key: String) -> String {
        let max_bare = MANAGED_MCP_NAME_MAX_CHARS - MANAGED_MCP_PREFIX.len();
        match key.char_indices().nth(max_bare) {
            Some((i, _)) => key.get(..i).map(str::to_string).unwrap_or(key),
            None => key,
        }
    }
    let mut pattern_key = key(pattern);
    let mut name_key = key(name);
    // Truncate only the runtime-truncated shape (managed name at exactly the cap), so a long entry never becomes a prefix grant over decoys
    // Residual: a decoy spelled exactly like the truncated name is string-identical and no matcher can tell them apart
    if name.starts_with(MANAGED_MCP_PREFIX) && name.chars().count() == MANAGED_MCP_NAME_MAX_CHARS {
        pattern_key = truncate(pattern_key);
        name_key = truncate(name_key);
    }
    !pattern_key.is_empty() && pattern_key == name_key
}

/// MCP policy across all sources: any deny wins; every restricted source
/// must allow; managed-only requires a positive grant.
#[derive(Debug, Clone, Default)]
pub struct McpServerPolicy {
    pub sources: Vec<McpServerAllowlist>,
}

impl McpServerPolicy {
    /// A single-source policy (test construction across crates).
    pub fn single(allowlist: McpServerAllowlist) -> Self {
        Self {
            sources: vec![allowlist],
        }
    }

    pub fn is_restricted(&self) -> bool {
        self.sources.iter().any(McpServerAllowlist::is_restricted)
    }

    /// The sources whose restrictions bind a subject of `origin`.
    fn binding_sources(
        &self,
        origin: PolicySubjectOrigin,
    ) -> impl Iterator<Item = &McpServerAllowlist> {
        self.sources.iter().filter(move |s| s.binds(origin))
    }

    /// Lockdown active for a subject of `origin` (advisory lockdowns don't
    /// bind grok-native servers).
    pub fn managed_only(&self, origin: PolicySubjectOrigin) -> bool {
        self.binding_sources(origin)
            .any(McpServerAllowlist::managed_only)
    }

    pub fn is_server_denied(
        &self,
        server: &agent_client_protocol::McpServer,
        origin: PolicySubjectOrigin,
    ) -> bool {
        self.denying_source(server, origin).is_some()
    }

    /// The binding source whose denylist matches `server`, if any.
    pub fn denying_source(
        &self,
        server: &agent_client_protocol::McpServer,
        origin: PolicySubjectOrigin,
    ) -> Option<&McpServerAllowlist> {
        self.binding_sources(origin)
            .find(|s| s.is_server_denied(server))
    }

    /// Allow-entry grant satisfying a restriction owned by `restriction_ownership`. Every
    /// source is consulted regardless of its `authority`: a grant widens, it never binds.
    pub fn grants_exception(
        &self,
        server: &agent_client_protocol::McpServer,
        restriction_ownership: PolicyLayerOwnership,
    ) -> bool {
        self.sources.iter().any(|s| {
            restriction_ownership.accepts_grant_from(s.ownership) && s.matches_allow_entry(server)
        })
    }

    /// Denied by no binding source and blocked by none: delegates to
    /// [`Self::blocking_allow_source`] so decision and attribution never desync.
    pub fn is_server_allowed(
        &self,
        server: &agent_client_protocol::McpServer,
        origin: PolicySubjectOrigin,
    ) -> bool {
        !self.is_server_denied(server, origin)
            && self.blocking_allow_source(server, origin).is_none()
    }

    /// The binding source blocking a non-denied server (reason attribution): an
    /// unsatisfied lockdown first, else a restricted source excluding the server.
    pub fn blocking_allow_source(
        &self,
        server: &agent_client_protocol::McpServer,
        origin: PolicySubjectOrigin,
    ) -> Option<&McpServerAllowlist> {
        self.binding_sources(origin)
            .find(|s| s.managed_only() && !self.grants_exception(server, s.ownership))
            .or_else(|| {
                self.binding_sources(origin)
                    .find(|s| !s.allows_ignoring_managed_only(server))
            })
    }

    /// Total allow/deny entry count across sources (doctor summary).
    pub fn entry_count(&self) -> usize {
        self.sources
            .iter()
            .map(|s| s.entries.len() + s.deny_entries.len())
            .sum()
    }

    /// Paths of the sources contributing to this policy (diagnostics).
    pub fn source_paths(&self) -> Vec<&Path> {
        self.sources
            .iter()
            .filter_map(|s| s.source_path.as_deref())
            .collect()
    }
}
