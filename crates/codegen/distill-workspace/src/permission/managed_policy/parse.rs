//! Parsers for the managed policy keys (Claude camelCase JSON and grok
//! snake_case TOML-as-JSON): MCP allow/deny lists, marketplace lists, and
//! boolean pins.

use std::path::Path;

use tracing::warn;

use super::layer::PolicyLayerOwnership;
use super::marketplace::{ManagedMarketplace, ManagedMarketplaceKind};
use super::mcp::AllowedMcpServer;
use super::url_match::{warn_on_unmatchable_allow_url, warn_on_unmatchable_deny_url};

/// Read a boolean policy key by its accepted spellings from the source at `path`. A non-bool
/// value or a spelling conflict warns naming the source and applies the fail-closed value.
pub(super) fn policy_bool(
    json: &serde_json::Value,
    keys: &[&str],
    fail_closed: bool,
    path: &Path,
) -> Option<bool> {
    let (key, value) = match policy_field(json, keys) {
        PolicyKey::Absent => return None,
        PolicyKey::Malformed => {
            warn!(
                path = %path.display(),
                keys = ?keys,
                fail_closed,
                "policy key is spelled both ways with different values; applying the fail-closed value"
            );
            return Some(fail_closed);
        }
        PolicyKey::Present((key, value)) => (key, value),
    };
    match value.as_bool() {
        Some(b) => Some(b),
        None => {
            warn!(
                path = %path.display(),
                key,
                %value,
                fail_closed,
                "policy key must be a boolean; applying the fail-closed value"
            );
            Some(fail_closed)
        }
    }
}

/// `strictKnownMarketplaces` → allowlist URLs (`git`+`url`, `github`+`repo`; `ref`
/// tolerated). Empty-`Present` and `Malformed` lock down; unsupported entries warn.
pub(super) fn parse_strict_marketplaces(json: &serde_json::Value) -> PolicyKey<Vec<String>> {
    policy_array(
        json,
        &["strictKnownMarketplaces", "strict_known_marketplaces"],
    )
    .map(|arr| {
        arr.iter()
            .filter_map(|entry| match marketplace_source_from_json(entry) {
                Some(ManagedMarketplaceKind::Git { url, .. }) => Some(url),
                Some(ManagedMarketplaceKind::Local { .. }) | None => {
                    warn!(
                        entry = %entry,
                        "ignoring unsupported strictKnownMarketplaces entry; only git+url and github+repo sources are honored"
                    );
                    None
                }
            })
            .collect()
    })
}

/// Parsed `extraKnownMarketplaces`. A per-entry `autoUpdate: false` has no
/// granular grok equivalent, so it (or an unreadable entry) pins the GLOBAL auto-update off.
#[derive(Default)]
pub(super) struct ExtraMarketplaces {
    pub entries: Vec<ManagedMarketplace>,
    pub pin_auto_update_off: bool,
}

/// `extraKnownMarketplaces`: map of name → `{ source: {…}, autoUpdate? }`.
pub(super) fn parse_extra_marketplaces(
    json: &serde_json::Value,
    path: &Path,
    ownership: PolicyLayerOwnership,
) -> ExtraMarketplaces {
    let keys = &["extraKnownMarketplaces", "extra_known_marketplaces"];
    let (key, value) = match policy_field(json, keys) {
        PolicyKey::Absent => return ExtraMarketplaces::default(),
        PolicyKey::Malformed => {
            warn!(
                path = %path.display(),
                keys = ?keys,
                "policy key is spelled both ways with different values; no marketplace \
                 is registered and plugin auto-update is pinned off (any opt-out it \
                 carried is unknowable)"
            );
            return ExtraMarketplaces {
                pin_auto_update_off: true,
                ..Default::default()
            };
        }
        PolicyKey::Present((key, value)) => (key, value),
    };
    let mut out = ExtraMarketplaces::default();
    let Some(map) = value.as_object() else {
        warn!(
            path = %path.display(),
            key,
            %value,
            "policy key must be a table of marketplaces; no marketplace is \
             registered and plugin auto-update is pinned off (any opt-out it \
             carried is unknowable)"
        );
        out.pin_auto_update_off = true;
        return out;
    };
    for (name, entry) in map {
        // Fail-closed like the wrong-typed table: a non-object entry cannot be read for an opt-out.
        let Some(fields) = entry.as_object() else {
            warn!(
                path = %path.display(),
                name,
                %entry,
                "extraKnownMarketplaces entry must be a table; it registers nothing and \
                 plugin auto-update is pinned off (any opt-out it carried is unknowable)"
            );
            out.pin_auto_update_off = true;
            continue;
        };
        if policy_bool(entry, &["autoUpdate", "auto_update"], false, path) == Some(false) {
            warn!(
                path = %path.display(),
                name,
                "extraKnownMarketplaces entry disables auto-update; grok has no \
                 per-marketplace switch, so plugin auto-update is pinned off for all"
            );
            out.pin_auto_update_off = true;
        }
        let Some(kind) = fields.get("source").and_then(marketplace_source_from_json) else {
            warn!(
                name,
                "ignoring extraKnownMarketplaces entry without a supported source"
            );
            continue;
        };
        out.entries.push(ManagedMarketplace {
            name: name.clone(),
            kind,
            ownership,
        });
    }
    out
}

/// One Claude source object: `{source:"git",url,ref?}`, `{source:"github",
/// repo,ref?}` (→ clone URL), or `{source:"local",path}`; `branch` ≡ `ref`.
fn marketplace_source_from_json(entry: &serde_json::Value) -> Option<ManagedMarketplaceKind> {
    let git_ref = entry
        .get("ref")
        .or_else(|| entry.get("branch"))
        .and_then(|v| v.as_str())
        .map(String::from);
    match entry.get("source")?.as_str()? {
        "git" => Some(ManagedMarketplaceKind::Git {
            url: entry.get("url")?.as_str()?.to_string(),
            git_ref,
        }),
        "github" => Some(ManagedMarketplaceKind::Git {
            url: format!("https://github.com/{}.git", entry.get("repo")?.as_str()?),
            git_ref,
        }),
        "local" => Some(ManagedMarketplaceKind::Local {
            path: entry.get("path")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

/// Which MCP policy list a source key names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpPolicyList {
    Allow,
    Deny,
}

impl McpPolicyList {
    /// Accepted spellings (Claude camelCase, TOML snake_case).
    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Allow => &["allowedMcpServers", "allowed_mcp_servers"],
            Self::Deny => &["deniedMcpServers", "denied_mcp_servers"],
        }
    }
}

/// Policy keys read from each TOML layer (exempt from the shell's unknown-key scan). The global
/// plugin pin is grok-only; both its spellings are accepted for symmetry with the other keys.
pub const MANAGED_POLICY_CONFIG_KEYS: &[&str] = &[
    "allowedMcpServers",
    "allowed_mcp_servers",
    "deniedMcpServers",
    "denied_mcp_servers",
    "allowManagedMcpServersOnly",
    "allow_managed_mcp_servers_only",
    "enableAllProjectMcpServers",
    "enable_all_project_mcp_servers",
    "pluginAutoUpdate",
    "plugin_auto_update",
    "strictKnownMarketplaces",
    "strict_known_marketplaces",
    "extraKnownMarketplaces",
    "extra_known_marketplaces",
];

/// A key (source-level or per-entry) by its accepted spellings, as `(first present key, value)`.
/// Differing values across spellings = `Malformed` (never shadowing); the caller reports it.
fn policy_field<'a, 'k>(
    json: &'a serde_json::Value,
    keys: &[&'k str],
) -> PolicyKey<(&'k str, &'a serde_json::Value)> {
    let mut present = keys
        .iter()
        .filter_map(|key| json.get(key).map(|value| (*key, value)));
    let Some(first) = present.next() else {
        return PolicyKey::Absent;
    };
    if present.any(|(_, value)| value != first.1) {
        return PolicyKey::Malformed;
    }
    PolicyKey::Present(first)
}

/// One key's value in one source. Only `Absent` means "no restriction": a
/// `Malformed` key (wrong type, or spelled both ways with different values) fails closed.
pub(super) enum PolicyKey<T> {
    Absent,
    Present(T),
    Malformed,
}

impl<T> PolicyKey<T> {
    fn and_then<U>(self, f: impl FnOnce(T) -> PolicyKey<U>) -> PolicyKey<U> {
        match self {
            PolicyKey::Absent => PolicyKey::Absent,
            PolicyKey::Present(value) => f(value),
            PolicyKey::Malformed => PolicyKey::Malformed,
        }
    }

    fn map<U>(self, f: impl FnOnce(T) -> U) -> PolicyKey<U> {
        self.and_then(|value| PolicyKey::Present(f(value)))
    }

    pub(super) fn is_absent(&self) -> bool {
        matches!(self, PolicyKey::Absent)
    }

    pub(super) fn is_malformed(&self) -> bool {
        matches!(self, PolicyKey::Malformed)
    }
}

impl<T> PolicyKey<Vec<T>> {
    /// Present-but-empty (lockdown) or malformed (block on error).
    pub(super) fn locks_down(&self) -> bool {
        matches!(self, PolicyKey::Present(e) if e.is_empty()) || self.is_malformed()
    }

    /// The parsed entries; `Absent` and `Malformed` carry none, so check
    /// [`Self::locks_down`] / [`Self::is_malformed`] first.
    pub(super) fn entries(self) -> Vec<T> {
        match self {
            PolicyKey::Present(entries) => entries,
            PolicyKey::Absent | PolicyKey::Malformed => Vec::new(),
        }
    }
}

/// Read an array policy key by its accepted spellings; a non-array value (TOML
/// admins write `key = { … }` or a bare string easily) is `Malformed`.
fn policy_array<'a>(
    json: &'a serde_json::Value,
    keys: &[&str],
) -> PolicyKey<&'a Vec<serde_json::Value>> {
    let (key, value) = match policy_field(json, keys) {
        PolicyKey::Absent => return PolicyKey::Absent,
        PolicyKey::Malformed => {
            warn!(
                keys = ?keys,
                "policy key is spelled both ways with different values; failing closed"
            );
            return PolicyKey::Malformed;
        }
        PolicyKey::Present((key, value)) => (key, value),
    };
    match value.as_array() {
        Some(arr) => PolicyKey::Present(arr),
        None => {
            warn!(
                key,
                %value,
                "policy key must be an array of entries; failing closed"
            );
            PolicyKey::Malformed
        }
    }
}

/// Parse one allow/deny list.
pub(super) fn parse_mcp_entry_list(
    json: &serde_json::Value,
    list: McpPolicyList,
) -> PolicyKey<Vec<AllowedMcpServer>> {
    policy_array(json, list.keys()).and_then(|arr| {
        parse_mcp_entries(arr, list).map_or(PolicyKey::Malformed, PolicyKey::Present)
    })
}

/// Unusable allow entries drop (they grant nothing); an unusable deny entry would
/// block nothing, so the whole key fails closed (`None`) once every entry is reported.
fn parse_mcp_entries(
    arr: &[serde_json::Value],
    list: McpPolicyList,
) -> Option<Vec<AllowedMcpServer>> {
    let mut entries = Vec::new();
    let mut enforceable = true;
    for entry in arr {
        match (parse_mcp_entry(entry, list), list) {
            (Some(parsed), _) => entries.push(parsed),
            (None, McpPolicyList::Deny) => {
                warn!(
                    entry = %entry,
                    "unenforceable deniedMcpServers entry; failing closed (no recognized field, a wrong-typed or ambiguously spelled field, or an unmatchable URL; honored fields: serverUrl, command, serverCommand, serverName)"
                );
                enforceable = false;
            }
            (None, McpPolicyList::Allow) => warn!(
                entry = %entry,
                "ignoring unusable allowedMcpServers entry; it grants nothing (no recognized field, or a wrong-typed or ambiguously spelled field; honored fields: serverUrl, command, serverCommand, serverName)"
            ),
        }
    }
    enforceable.then_some(entries)
}

/// Entry fields' accepted spellings (`command` has one).
const SERVER_URL: &[&str] = &["serverUrl", "server_url"];
const SERVER_COMMAND: &[&str] = &["serverCommand", "server_command"];
const SERVER_NAME: &[&str] = &["serverName", "server_name"];

/// `serverUrl` → Http, `serverCommand` → StdioArgv, `command` → Stdio, `serverName` → Name.
/// `None`: unknown shape, a field spelled both ways with different values, or an unmatchable deny URL.
fn parse_mcp_entry(entry: &serde_json::Value, list: McpPolicyList) -> Option<AllowedMcpServer> {
    if [SERVER_URL, SERVER_COMMAND, SERVER_NAME]
        .iter()
        .any(|keys| policy_field(entry, keys).is_malformed())
    {
        return None;
    }
    let field = |keys: &[&str]| keys.iter().find_map(|key| entry.get(key));
    if let Some(url) = field(SERVER_URL).and_then(|u| u.as_str()) {
        let unmatchable = match list {
            McpPolicyList::Deny => warn_on_unmatchable_deny_url(url),
            McpPolicyList::Allow => {
                warn_on_unmatchable_allow_url(url);
                false
            }
        };
        return (!unmatchable).then(|| AllowedMcpServer::Http {
            url_pattern: url.to_string(),
        });
    }
    if let Some(argv) = field(SERVER_COMMAND)
        .and_then(|c| c.as_array())
        // All-or-nothing: a partial argv would match the wrong command.
        .and_then(|a| {
            a.iter()
                .map(|v| v.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()
        })
        .filter(|argv| !argv.is_empty())
    {
        return Some(AllowedMcpServer::StdioArgv { argv });
    }
    if let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) {
        return Some(AllowedMcpServer::Stdio {
            command: cmd.to_string(),
        });
    }
    field(SERVER_NAME)
        .and_then(|n| n.as_str())
        .map(|name| AllowedMcpServer::Name {
            name: name.to_string(),
        })
}
