// Modified for Distill by Samuel Fajreldines, 2026.
//! Shell-side MCP merge: local/plugin/compat sources plus admitted client servers.
//! Managed connectors exist only via the gateway catalog (`GET /v1/mcp/tools/list`), not as injected `grok_com_*` HTTP servers.
//!
//! Merge layers are applied in order, keyed by server NAME (two names sharing one URL are distinct servers).
//! Later `insert()` beats earlier `or_insert()`:
//!   - config.toml    — seeds the map; `enabled = false` blocks lower layers
//!   - Plugins        — `or_insert` (won't override config.toml)
//!   - ~/.claude.json — `or_insert` (imported user/local MCP servers)
//!   - `.mcp.json`    — `or_insert` (team baseline)
//!   - Client         — `insert` (wins except servers rejected by a disabled
//!                      vendor `mcps` kill switch, which matches by normalized
//!                      URL; see `admit_client_mcp_servers`)
//!
//! The gateway catalog/call core lives in `distill_shell_session_support::managed_mcp`.
//! It is re-exported here so `crate::session::managed_mcp::…` paths keep resolving unchanged.

pub use distill_shell_session_support::managed_mcp::*;

use std::collections::HashMap;

use agent_client_protocol as acp;
use distill_workspace::permission::resolution::{
    McpBlockReason, McpSubject, McpVerdict, PolicySubjectOrigin,
};

/// Vendor kill-switch attribution key: normalized URL for Http/Sse (a client re-forwards the same endpoint under any display name), name for Stdio.
/// Only for [`admit_client_mcp_servers`]; merge/discovery maps key by name.
fn mcp_vendor_block_key(s: &acp::McpServer) -> String {
    match s {
        acp::McpServer::Http(acp::McpServerHttp { url, .. })
        | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => normalize_url(url),
        acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) => name.clone(),
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => String::new(),
    }
}

pub(crate) fn mcp_server_name(s: &acp::McpServer) -> &str {
    match s {
        acp::McpServer::Http(acp::McpServerHttp { name, .. })
        | acp::McpServer::Sse(acp::McpServerSse { name, .. })
        | acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) => name,
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => "",
    }
}

/// Merge/discovery map key: server NAME is the sole merge identity (two names sharing one URL are distinct servers).
/// Every name-keyed map in this module must derive its key through this helper so merge and discovery keying cannot desynchronize.
fn mcp_merge_key(s: &acp::McpServer) -> String {
    mcp_server_name(s).to_string()
}

/// Whole-definition equality. Not the derived `==` directly: `env`/`headers` come from HashMap
/// iteration, so two loads of one TOML differ in order; `args` order stays significant.
fn mcp_server_definitions_equal(a: &acp::McpServer, b: &acp::McpServer) -> bool {
    canonical_definition(a) == canonical_definition(b)
}

fn canonical_definition(server: &acp::McpServer) -> acp::McpServer {
    let mut server = server.clone();
    match &mut server {
        acp::McpServer::Stdio(s) => s
            .env
            .sort_by(|a, b| (&a.name, &a.value).cmp(&(&b.name, &b.value))),
        acp::McpServer::Http(s) => s
            .headers
            .sort_by(|a, b| (&a.name, &a.value).cmp(&(&b.name, &b.value))),
        acp::McpServer::Sse(s) => s
            .headers
            .sort_by(|a, b| (&a.name, &a.value).cmp(&(&b.name, &b.value))),
        // `McpServer` is #[non_exhaustive]; unknown transports have nothing to canonicalize.
        _ => {}
    }
    server
}

pub(crate) fn merge_managed_mcp_servers(
    client_mcp_servers: Vec<acp::McpServer>,
    cwd: &std::path::Path,
    plugin_registry: Option<&distill_agent::plugins::PluginRegistry>,
    compat: &distill_tools::types::compat::CompatConfig,
) -> Vec<acp::McpServer> {
    merge_managed_mcp_servers_with_policy(client_mcp_servers, cwd, plugin_registry, compat)
        .into_iter()
        .filter(|s| s.disabled_reason.is_none())
        .map(|s| s.server)
        .collect()
}

/// Merge local/plugin/client MCP sources into ONE live session and push the result via [`crate::session::SessionCommand::UpdateMcpServers`].
/// Returns `true` if the command was enqueued (session still alive).
/// Shared core for every "re-merge MCP sources into a live session" path (config hot-reload, post-grant reload, plugin reload).
pub(crate) fn merge_and_send_managed_mcp_update(
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<crate::session::SessionCommand>,
    cwd: &std::path::Path,
    initial_client_mcp_servers: Vec<acp::McpServer>,
    plugin_registry: Option<&distill_agent::plugins::PluginRegistry>,
    compat: &distill_tools::types::compat::CompatConfig,
) -> bool {
    let merged =
        merge_managed_mcp_servers(initial_client_mcp_servers, cwd, plugin_registry, compat);
    let (tx, _rx) = tokio::sync::oneshot::channel();
    cmd_tx
        .send(crate::session::SessionCommand::UpdateMcpServers {
            mcp_servers: merged,
            respond_to: tx,
        })
        .is_ok()
}

/// Drop client-forwarded servers that match on-disk vendor MCP configs while that vendor's `mcps` kill switch is off.
/// Call at session ingress before storing the hot-reload seed (`initial_client_mcp_servers`).
/// Explicit later client updates re-run this with current disk, so a server that no longer matches a disabled vendor's config can be admitted.
pub(crate) fn admit_client_mcp_servers(
    client_mcp_servers: Vec<acp::McpServer>,
    cwd: &std::path::Path,
    compat: &distill_tools::types::compat::CompatConfig,
) -> Vec<acp::McpServer> {
    let mut blocked: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !compat.cursor.mcps {
        let mut forced = *compat;
        forced.cursor.mcps = true;
        blocked.extend(
            crate::util::config::load_cursor_mcp_servers(cwd, &forced)
                .iter()
                .map(mcp_vendor_block_key),
        );
    }
    if !compat.claude.mcps {
        // Attribution must see disk even when import-marker / runtime gates empty the normal Claude loader
        blocked.extend(
            crate::util::config::load_claude_json_mcp_servers_for_attribution(cwd)
                .iter()
                .map(mcp_vendor_block_key),
        );
    }
    if blocked.is_empty() {
        return client_mcp_servers;
    }
    client_mcp_servers
        .into_iter()
        .filter(|s| !blocked.contains(&mcp_vendor_block_key(s)))
        .collect()
}

pub(crate) fn merge_managed_mcp_servers_with_policy(
    client_mcp_servers: Vec<acp::McpServer>,
    cwd: &std::path::Path,
    plugin_registry: Option<&distill_agent::plugins::PluginRegistry>,
    compat: &distill_tools::types::compat::CompatConfig,
) -> Vec<McpServerWithPolicy> {
    merge_managed_mcp_servers_with_policy_from(
        client_mcp_servers,
        cwd,
        plugin_registry,
        compat,
        distill_workspace::permission::resolution::managed_settings(),
    )
}

/// [`merge_managed_mcp_servers_with_policy`] over injected settings — the OnceLock test seam that
/// lets a test drive the real merge glue with a constructed policy.
fn merge_managed_mcp_servers_with_policy_from(
    client_mcp_servers: Vec<acp::McpServer>,
    cwd: &std::path::Path,
    plugin_registry: Option<&distill_agent::plugins::PluginRegistry>,
    compat: &distill_tools::types::compat::CompatConfig,
    ms: &distill_workspace::permission::resolution::ManagedSettings,
) -> Vec<McpServerWithPolicy> {
    // Project-scoped names classify their servers foreign AND feed the
    // project-MCP pin; computed once for both (via `mcp_subject`).
    let project = crate::agent::folder_trust::project_scoped_mcp_names(cwd);
    // Server and subject share one map entry, so a key collision can never
    // pair a surviving server with another definition's subject.
    let mut servers: HashMap<String, (acp::McpServer, McpSubject)> =
        merge_managed_mcp_servers_sourced(cwd, plugin_registry, compat)
            .into_iter()
            .map(|(s, source)| {
                let subject = mcp_subject(&s, &source, &project);
                (mcp_merge_key(&s), (s, subject))
            })
            .collect();

    // Re-admit at merge so a caller that forgot ingress sanitization cannot spawn disabled-vendor
    // servers; subject inheritance needs the SAME definition, else foreign (fail closed).
    for server in admit_client_mcp_servers(client_mcp_servers, cwd, compat) {
        let key = mcp_merge_key(&server);
        let subject = match servers.get(&key) {
            Some((native, subject)) if mcp_server_definitions_equal(native, &server) => *subject,
            _ => McpSubject {
                origin: PolicySubjectOrigin::Foreign,
                project_scoped: project.contains(mcp_server_name(&server)),
            },
        };
        servers.insert(key, (server, subject));
    }

    let disabled = crate::util::config::disabled_mcp_server_names(cwd);

    let mut merged: Vec<(acp::McpServer, McpSubject)> = servers.into_values().collect();
    // Sort by name: HashMap order is random, and the order-sensitive downstream equality would
    // see a no-op reload as changed (spuriously restarting MCP init).
    merged.sort_by(|a, b| mcp_server_name(&a.0).cmp(mcp_server_name(&b.0)));
    // Folder-trust gate: an untrusted workspace's project-scoped servers drop before spawn;
    // composes with the managed policy applied next.
    let merged = crate::agent::folder_trust::filter_untrusted_project_mcp_with(
        cwd,
        merged,
        &project,
        |(server, _)| mcp_server_name(server),
    );
    apply_mcp_server_policy(merged, &disabled, ms)
}

/// Classify what defined a server for policy scoping: config.toml and plugin servers are
/// grok-native, the rest foreign; `project_names` reclassifies collisions foreign (fail closed).
pub(crate) fn mcp_subject(
    server: &acp::McpServer,
    source: &distill_tools::types::config_source::ConfigSource,
    project_names: &std::collections::HashSet<String>,
) -> McpSubject {
    use distill_tools::types::config_source::ConfigSource;
    mcp_subject_for_tier(
        mcp_server_name(server),
        matches!(
            source,
            ConfigSource::ConfigToml { .. } | ConfigSource::Plugin { .. }
        ),
        project_names,
    )
}

/// The ONE origin classifier (merge, discovery, agent pool): native tier unless the name is
/// project-claimed — a collision classifies foreign (fail closed).
pub(crate) fn mcp_subject_for_tier(
    server_name: &str,
    native_tier: bool,
    project_names: &std::collections::HashSet<String>,
) -> McpSubject {
    let project_scoped = project_names.contains(server_name);
    let native = native_tier && !project_scoped;
    McpSubject {
        origin: if native {
            PolicySubjectOrigin::GrokNative
        } else {
            PolicySubjectOrigin::Foreign
        },
        project_scoped,
    }
}

/// Project-pin gate shared by the merge and discovery: drop + warn when the
/// `enableAllProjectMcpServers = false` pin blocks `server` (pin before verdict).
fn dropped_by_project_pin(
    ms: &distill_workspace::permission::resolution::ManagedSettings,
    server: &acp::McpServer,
    subject: McpSubject,
) -> bool {
    if ms.mcp_project_pin_block(server, subject).is_none() {
        return false;
    }
    tracing::warn!(
        server = %mcp_server_name(server),
        source = %ms
            .project_mcp
            .source()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "project-level MCP disabled by managed policy (enableAllProjectMcpServers = false)"
    );
    true
}

/// An MCP server paired with its policy status.
pub(crate) struct McpServerWithPolicy {
    pub server: acp::McpServer,
    pub disabled_reason: Option<McpBlockReason>,
}

/// The documented admin signal for a policy-dropped server — written to the always-on
/// unified.jsonl as well as tracing, which alone reaches no file in a default run.
fn log_policy_block(name: &str, reason: &McpBlockReason) {
    tracing::warn!(
        name,
        reason = %reason,
        "MCP server blocked by managed settings policy"
    );
    distill_telemetry::unified_log::warn(
        "MCP server blocked by managed settings policy",
        None,
        Some(serde_json::json!({ "server": name, "reason": reason.to_string() })),
    );
}

/// Apply the managed MCP policy to the merged list: drop config-disabled and project-pinned
/// servers, tag everything [`ManagedSettings::mcp_verdict`] blocks; servers stay subject-paired.
fn apply_mcp_server_policy(
    merged: Vec<(acp::McpServer, McpSubject)>,
    disabled: &std::collections::HashSet<String>,
    ms: &distill_workspace::permission::resolution::ManagedSettings,
) -> Vec<McpServerWithPolicy> {
    merged
        .into_iter()
        .filter_map(|(server, subject)| {
            if disabled.contains(mcp_server_name(&server)) {
                return None;
            }
            if dropped_by_project_pin(ms, &server, subject) {
                return None;
            }
            match ms.mcp_verdict(&server, subject) {
                McpVerdict::Blocked(reason) => {
                    log_policy_block(mcp_server_name(&server), &reason);
                    Some(McpServerWithPolicy {
                        server,
                        disabled_reason: Some(reason),
                    })
                }
                McpVerdict::Allowed => Some(McpServerWithPolicy {
                    server,
                    disabled_reason: None,
                }),
            }
        })
        .collect()
}

/// Policy gate for the session-less MCP pool (without it `x.ai/mcp/call` spawns blocked servers);
/// native tier needs the payload to EQUAL the on-disk TOML definition, as in the session merge.
pub(crate) fn filter_policy_blocked_agent_mcp(
    servers: Vec<acp::McpServer>,
    cwd: &std::path::Path,
) -> Vec<acp::McpServer> {
    if servers.is_empty() {
        return servers;
    }
    let toml_servers: HashMap<String, acp::McpServer> =
        crate::util::config::load_mcp_servers_toml_only(cwd)
            .into_iter()
            .map(|s| (mcp_merge_key(&s), s))
            .collect();
    filter_policy_blocked_agent_mcp_with(
        servers,
        &toml_servers,
        &crate::agent::folder_trust::project_scoped_mcp_names(cwd),
        distill_workspace::permission::resolution::managed_settings(),
    )
}

/// [`filter_policy_blocked_agent_mcp`] over injected inputs (the OnceLock
/// seam — see [`merge_managed_mcp_servers_with_policy_from`]).
fn filter_policy_blocked_agent_mcp_with(
    servers: Vec<acp::McpServer>,
    toml_servers: &HashMap<String, acp::McpServer>,
    project_names: &std::collections::HashSet<String>,
    ms: &distill_workspace::permission::resolution::ManagedSettings,
) -> Vec<acp::McpServer> {
    servers
        .into_iter()
        .filter(|server| {
            let name = mcp_server_name(server);
            // Name ownership alone is not enough: a squatted TOML name stays foreign (fail closed).
            let native = toml_servers
                .get(name)
                .is_some_and(|disk| mcp_server_definitions_equal(disk, server));
            let subject = mcp_subject_for_tier(name, native, project_names);
            if dropped_by_project_pin(ms, server, subject) {
                return false;
            }
            match ms.mcp_verdict(server, subject) {
                McpVerdict::Allowed => true,
                McpVerdict::Blocked(reason) => {
                    log_policy_block(name, &reason);
                    false
                }
            }
        })
        .collect()
}

/// Managed-policy block reasons keyed by server name (first writer wins) — the ONE reasons-map
/// assembly for doctor and `mcp/list`, so CLI verdicts can't drift from the merge.
pub(crate) fn mcp_blocked_reasons<'a>(
    definitions: impl IntoIterator<Item = (&'a str, &'a acp::McpServer, McpSubject)>,
    ms: &distill_workspace::permission::resolution::ManagedSettings,
) -> HashMap<String, McpBlockReason> {
    let mut out = HashMap::new();
    for (name, server, subject) in definitions {
        if let McpVerdict::Blocked(reason) = ms.mcp_verdict(server, subject) {
            out.entry(name.to_string()).or_insert(reason);
        }
    }
    out
}

/// Like [`merge_managed_mcp_servers`] but returns `ConfigSource` alongside each server.
pub(crate) fn merge_managed_mcp_servers_sourced(
    cwd: &std::path::Path,
    plugin_registry: Option<&distill_agent::plugins::PluginRegistry>,
    compat: &distill_tools::types::compat::CompatConfig,
) -> Vec<(
    acp::McpServer,
    distill_tools::types::config_source::ConfigSource,
)> {
    let _mcp_merge_timer = crate::instrumentation::timer("mcp_merge_managed");
    use distill_tools::types::config_source::ConfigSource;

    let toml_claimed_names = crate::util::config::all_toml_mcp_server_names(cwd);

    let config_source = ConfigSource::ConfigToml {
        path: distill_tools::util::distill_home::distill_home().join("config.toml"),
    };

    // Use the TOML-only loader so that entries from imported editor configs and .mcp.json are not pre-loaded with ConfigSource::ConfigToml
    let mut servers: HashMap<String, (acp::McpServer, ConfigSource)> =
        crate::util::config::load_mcp_servers_toml_only(cwd)
            .into_iter()
            .map(|s| {
                let key = mcp_merge_key(&s);
                (key, (s, config_source.clone()))
            })
            .collect();
    for (name, (_, source)) in &servers {
        tracing::debug!(server = name, source = ?source, "MCP server loaded from source");
    }

    for (server, source) in
        non_toml_mcp_servers_with_source(cwd, plugin_registry, compat, &toml_claimed_names)
    {
        servers
            .entry(mcp_merge_key(&server))
            .or_insert((server, source));
    }

    servers.into_values().collect()
}

/// Plugin / Claude / Cursor / `.mcp.json` servers in merge priority order.
/// Callers insert with `entry(name).or_insert` so the first listed source wins a shared name.
/// TOML is applied separately (last-wins for merge and for discovery force-enable).
fn non_toml_mcp_servers_with_source(
    cwd: &std::path::Path,
    plugin_registry: Option<&distill_agent::plugins::PluginRegistry>,
    compat: &distill_tools::types::compat::CompatConfig,
    toml_claimed_names: &std::collections::HashSet<String>,
) -> Vec<(
    acp::McpServer,
    distill_tools::types::config_source::ConfigSource,
)> {
    use distill_tools::types::config_source::ConfigSource;

    let mut out = Vec::new();

    if let Some(registry) = plugin_registry {
        for plugin in registry.active_plugins() {
            let mut plugin_servers: Vec<acp::McpServer> = Vec::new();
            if let Some(ref mcp_path) = plugin.mcp_config_path {
                let (servers, _) = load_plugin_mcp_servers(
                    mcp_path,
                    &plugin.name,
                    &plugin.root_str(),
                    &plugin.data_dir_str(),
                );
                plugin_servers.extend(servers);
            }
            if let Some(ref inline_value) = plugin.inline_mcp_servers {
                let (servers, _) = load_plugin_mcp_servers_from_value(
                    inline_value,
                    &plugin.name,
                    &plugin.root_str(),
                    &plugin.data_dir_str(),
                );
                plugin_servers.extend(servers);
            }
            if plugin_servers.is_empty() {
                continue;
            }
            let mut seen_names: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            plugin_servers.retain(|server| seen_names.insert(mcp_server_name(server).to_string()));
            let source = ConfigSource::Plugin {
                plugin_name: plugin.name.clone(),
                path: plugin.root.clone(),
            };
            for server in plugin_servers {
                if toml_claimed_names.contains(mcp_server_name(&server)) {
                    continue;
                }
                out.push((server, source.clone()));
            }
        }
    }

    let claude_json_source = ConfigSource::ClaudeJson {
        path: distill_dirs::home_dir()
            .map(|h| h.join(".claude.json"))
            .unwrap_or_default(),
    };
    for server in crate::util::config::load_claude_json_mcp_servers(cwd, compat) {
        if toml_claimed_names.contains(mcp_server_name(&server)) {
            continue;
        }
        out.push((server, claude_json_source.clone()));
    }

    let cursor_mcp_source = ConfigSource::McpJson {
        path: distill_dirs::home_dir()
            .map(|h| h.join(".cursor").join("mcp.json"))
            .unwrap_or_default(),
    };
    for server in crate::util::config::load_cursor_mcp_servers(cwd, compat) {
        if toml_claimed_names.contains(mcp_server_name(&server)) {
            continue;
        }
        out.push((server, cursor_mcp_source.clone()));
    }

    let mcp_json_source = ConfigSource::McpJson {
        path: cwd.join(".mcp.json"),
    };
    for server in crate::util::config::load_mcp_json_servers(cwd) {
        if toml_claimed_names.contains(mcp_server_name(&server)) {
            continue;
        }
        out.push((server, mcp_json_source.clone()));
    }

    out
}

/// Shared inputs for MCP definition discovery (list stubs and setup probe).
#[derive(Clone, Copy)]
pub(crate) struct McpDiscoveryInputs<'a> {
    pub cwd: &'a std::path::Path,
    pub plugin_registry: Option<&'a distill_agent::plugins::PluginRegistry>,
    pub compat: &'a distill_tools::types::compat::CompatConfig,
}

/// Definitions that would exist if personal disable were cleared.
/// TOML `enabled = false` is force-enabled for stubs.
/// Returns transports keyed by server name.
pub(crate) fn discover_mcp_definitions_ignoring_disable(
    inputs: &McpDiscoveryInputs<'_>,
) -> HashMap<String, (acp::McpServer, McpSubject)> {
    use crate::util::config::{
        McpEnabledFilter, load_mcp_preferences, load_mcp_server_configs_with_project,
        materialize_mcp_config,
    };

    let cwd = inputs.cwd;
    let plugin_registry = inputs.plugin_registry;
    let compat = inputs.compat;

    let preferences = load_mcp_preferences().file();
    let sub = &crate::config::expand_env_vars_in_string;
    let toml_claimed = crate::util::config::all_toml_mcp_server_names(cwd);

    // TOML wins its name (insert); lower tiers or_insert. Subjects come from the merge's
    // classifier, so setup/stub verdicts can't diverge on a name collision.
    let project = crate::agent::folder_trust::project_scoped_mcp_names(cwd);
    let mut by_name: HashMap<String, (acp::McpServer, McpSubject)> = HashMap::new();
    for (name, (config, scope)) in load_mcp_server_configs_with_project(cwd) {
        let Some(transport) =
            materialize_mcp_config(&name, config, &preferences, sub, McpEnabledFilter::Ignore)
        else {
            continue;
        };
        let subject = mcp_subject_for_tier(
            &name,
            scope != crate::util::config::MCP_SCOPE_PROJECT,
            &project,
        );
        by_name.insert(name, (transport, subject));
    }
    for (server, source) in
        non_toml_mcp_servers_with_source(cwd, plugin_registry, compat, &toml_claimed)
    {
        let subject = mcp_subject(&server, &source, &project);
        by_name
            .entry(mcp_merge_key(&server))
            .or_insert((server, subject));
    }

    let entries: Vec<(acp::McpServer, McpSubject)> = by_name.into_values().collect();
    let entries = crate::agent::folder_trust::filter_untrusted_project_mcp_with(
        cwd,
        entries,
        &project,
        |(server, _)| mcp_server_name(server),
    );
    // Project-pin-dropped definitions stay: consumers gate through `mcp_verdict`, whose ProjectPin
    // leg names the pinning source — dropping them misreported "not found in config".
    entries
        .into_iter()
        .map(|(server, subject)| (mcp_merge_key(&server), (server, subject)))
        .collect()
}

fn load_plugin_mcp_servers(
    mcp_path: &std::path::Path,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<acp::McpServer>, crate::util::config::McpOAuthConfigMap) {
    let Some(config) = crate::util::config::read_mcp_json(mcp_path) else {
        return (vec![], crate::util::config::McpOAuthConfigMap::new());
    };
    load_plugin_mcp_servers_from_config(&config, plugin_name, plugin_root, plugin_data)
}

/// Like [`load_plugin_mcp_servers`] but from an in-memory JSON value (no I/O).
fn load_plugin_mcp_servers_from_value(
    root: &serde_json::Value,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<acp::McpServer>, crate::util::config::McpOAuthConfigMap) {
    let normalized = distill_agent::plugins::manifest::normalize_inline_mcp_servers(root);
    let Ok(config) = serde_json::from_value::<crate::util::config::McpConfig>(normalized) else {
        tracing::warn!(plugin = plugin_name, "failed to parse plugin MCP config");
        return (vec![], crate::util::config::McpOAuthConfigMap::new());
    };
    load_plugin_mcp_servers_from_config(&config, plugin_name, plugin_root, plugin_data)
}

fn load_plugin_mcp_servers_from_config(
    config: &crate::util::config::McpConfig,
    plugin_name: &str,
    plugin_root: &str,
    plugin_data: &str,
) -> (Vec<acp::McpServer>, crate::util::config::McpOAuthConfigMap) {
    let sub = |s: &str| -> String {
        let s = distill_agent::plugins::manifest::substitute_env_vars(s, plugin_root, plugin_data);
        crate::config::expand_env_vars_in_string(&s)
    };
    let label = format!("plugin:{}", plugin_name);
    crate::util::config::parse_mcp_config_with_oauth(config, &label, &sub)
}

pub(crate) fn collect_plugin_oauth_configs(
    plugin_registry: Option<&distill_agent::plugins::PluginRegistry>,
) -> crate::util::config::McpOAuthConfigMap {
    let mut oauth_configs = crate::util::config::McpOAuthConfigMap::new();
    let Some(registry) = plugin_registry else {
        return oauth_configs;
    };

    for plugin in registry.active_plugins() {
        if let Some(ref mcp_path) = plugin.mcp_config_path {
            let (_, oauth) = load_plugin_mcp_servers(
                mcp_path,
                &plugin.name,
                &plugin.root_str(),
                &plugin.data_dir_str(),
            );
            for (name, cfg) in oauth {
                oauth_configs.entry(name).or_insert(cfg);
            }
        }
        if let Some(ref inline_value) = plugin.inline_mcp_servers {
            let (_, oauth) = load_plugin_mcp_servers_from_value(
                inline_value,
                &plugin.name,
                &plugin.root_str(),
                &plugin.data_dir_str(),
            );
            for (name, cfg) in oauth {
                oauth_configs.entry(name).or_insert(cfg);
            }
        }
    }

    oauth_configs
}

pub(crate) fn merge_plugin_oauth_into(
    oauth_config_map: &mut crate::util::config::McpOAuthConfigMap,
    plugin_oauth: crate::util::config::McpOAuthConfigMap,
    toml_mcp_names: &std::collections::HashSet<String>,
) {
    for (name, cfg) in plugin_oauth {
        if toml_mcp_names.contains(&name) {
            continue;
        }
        oauth_config_map.insert(name, cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at<T>(xs: &[T], i: usize) -> &T {
        let Some(x) = xs.get(i) else {
            panic!("expected index {i}, len {}", xs.len());
        };
        x
    }

    fn empty_cwd() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// Injected settings carrying just an MCP policy (the OnceLock seam).
    fn settings_with_policy(
        policy: distill_workspace::permission::resolution::McpServerPolicy,
    ) -> distill_workspace::permission::resolution::ManagedSettings {
        let mut ms = distill_workspace::permission::resolution::ManagedSettings::default();
        ms.mcp_allowlist = policy;
        ms
    }

    /// Pair a server with a foreign, non-project subject (every policy binds).
    fn foreign(server: acp::McpServer) -> (acp::McpServer, McpSubject) {
        (
            server,
            McpSubject {
                origin: PolicySubjectOrigin::Foreign,
                project_scoped: false,
            },
        )
    }

    /// The merge must keep client-provided servers (they exist in no on-disk config), or hot-reloads tear them down.
    #[test]
    fn client_provided_servers_survive_merge() {
        let client = vec![acp::McpServer::Http(
            acp::McpServerHttp::new(
                "demo-mcp".to_string(),
                "http://mcp.example.test/api/mcp".to_string(),
            )
            .headers(vec![]),
        )];
        let cwd = empty_cwd();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(client, cwd.path(), None, &compat);
        assert!(
            merged.iter().any(|s| matches!(
                s,
                acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "demo-mcp"
            )),
            "client-provided server must survive a merge with no disk/managed sources"
        );
    }

    fn write_cursor_project_mcp(cwd: &std::path::Path, name: &str) {
        std::fs::create_dir_all(cwd.join(".cursor")).unwrap();
        std::fs::write(
            cwd.join(".cursor").join("mcp.json"),
            format!(r#"{{"mcpServers": {{"{name}": {{"command": "true"}}}}}}"#),
        )
        .unwrap();
    }

    fn client_stdio(name: &str) -> acp::McpServer {
        acp::McpServer::Stdio(acp::McpServerStdio::new(name.to_string(), "true"))
    }

    /// Vendor mcps kill switch must drop client-forwarded servers that match on-disk vendor config (pager may still load with default-on compat).
    #[test]
    fn client_cursor_server_dropped_when_cursor_mcps_disabled() {
        let cwd = empty_cwd();
        write_cursor_project_mcp(cwd.path(), "killswitch-cache");
        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.cursor.mcps = false;
        let merged = merge_managed_mcp_servers(
            vec![client_stdio("killswitch-cache")],
            cwd.path(),
            None,
            &compat,
        );
        assert!(
            !merged
                .iter()
                .any(|s| mcp_server_name(s) == "killswitch-cache"),
            "client-forwarded cursor server must be dropped when cursor.mcps is off"
        );
    }

    #[test]
    fn client_cursor_server_kept_when_cursor_mcps_enabled() {
        let cwd = empty_cwd();
        write_cursor_project_mcp(cwd.path(), "killswitch-cache");
        let compat = distill_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(
            vec![client_stdio("killswitch-cache")],
            cwd.path(),
            None,
            &compat,
        );
        assert!(
            merged
                .iter()
                .any(|s| mcp_server_name(s) == "killswitch-cache"),
            "client-forwarded cursor server must remain when cursor.mcps is on"
        );
    }

    #[test]
    fn unrelated_client_server_survives_when_cursor_mcps_disabled() {
        let cwd = empty_cwd();
        write_cursor_project_mcp(cwd.path(), "killswitch-cache");
        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.cursor.mcps = false;
        let merged = merge_managed_mcp_servers(
            vec![
                client_stdio("killswitch-cache"),
                client_stdio("client-only-binding"),
            ],
            cwd.path(),
            None,
            &compat,
        );
        assert!(
            !merged
                .iter()
                .any(|s| mcp_server_name(s) == "killswitch-cache"),
            "matching cursor client server must be dropped"
        );
        assert!(
            merged
                .iter()
                .any(|s| mcp_server_name(s) == "client-only-binding"),
            "unrelated client-only server must survive vendor kill switch"
        );
    }

    #[test]
    fn toml_claim_survives_when_client_cursor_insert_skipped() {
        let cwd = empty_cwd();
        write_cursor_project_mcp(cwd.path(), "killswitch-cache");
        std::fs::create_dir_all(cwd.path().join(".grok")).unwrap();
        std::fs::write(
            cwd.path().join(".grok").join("config.toml"),
            r#"
[mcp_servers.killswitch-cache]
command = "echo"
args = ["ok"]
"#,
        )
        .unwrap();
        git2::Repository::init(cwd.path()).unwrap();

        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.cursor.mcps = false;
        let merged = merge_managed_mcp_servers(
            vec![client_stdio("killswitch-cache")],
            cwd.path(),
            None,
            &compat,
        );
        let server = merged
            .iter()
            .find(|s| mcp_server_name(s) == "killswitch-cache")
            .expect("toml-claimed server must remain when client cursor insert is skipped");
        match server {
            acp::McpServer::Stdio(acp::McpServerStdio { command, args, .. }) => {
                assert_eq!(command.display().to_string(), "echo");
                assert_eq!(args.as_slice(), &["ok"]);
            }
            other => panic!("expected toml stdio server, got {other:?}"),
        }
    }

    /// Admitted seed must stay empty of the blocked server after vendor disk vanishes (hot-reload must not re-admit from a sanitized seed).
    #[test]
    fn admitted_seed_stays_blocked_after_vendor_disk_vanishes() {
        let cwd = empty_cwd();
        write_cursor_project_mcp(cwd.path(), "killswitch-cache");
        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.cursor.mcps = false;

        let admitted =
            admit_client_mcp_servers(vec![client_stdio("killswitch-cache")], cwd.path(), &compat);
        assert!(
            !admitted
                .iter()
                .any(|s| mcp_server_name(s) == "killswitch-cache"),
            "admit must drop matching vendor client server while flag is off"
        );

        std::fs::remove_file(cwd.path().join(".cursor").join("mcp.json")).unwrap();

        let merged = merge_managed_mcp_servers(admitted, cwd.path(), None, &compat);
        assert!(
            !merged
                .iter()
                .any(|s| mcp_server_name(s) == "killswitch-cache"),
            "admitted seed must not re-admit after vendor disk vanishes"
        );
    }

    /// Http/Sse client identity is normalized URL, not display name.
    #[test]
    fn client_cursor_http_dropped_by_normalized_url_when_mcps_disabled() {
        let cwd = empty_cwd();
        std::fs::create_dir_all(cwd.path().join(".cursor")).unwrap();
        std::fs::write(
            cwd.path().join(".cursor").join("mcp.json"),
            r#"{"mcpServers": {"disk-name": {"url": "https://killswitch.example.test/mcp/"}}}"#,
        )
        .unwrap();
        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.cursor.mcps = false;

        let matching = acp::McpServer::Http(
            acp::McpServerHttp::new(
                "killswitch-http".to_string(),
                "https://killswitch.example.test/mcp".to_string(),
            )
            .headers(vec![]),
        );
        let other = acp::McpServer::Http(
            acp::McpServerHttp::new(
                "other-http".to_string(),
                "https://other.example.test/mcp".to_string(),
            )
            .headers(vec![]),
        );
        let merged = merge_managed_mcp_servers(vec![matching, other], cwd.path(), None, &compat);
        assert!(
            !merged
                .iter()
                .any(|s| mcp_server_name(s) == "killswitch-http"),
            "same normalized URL with different name must be dropped"
        );
        assert!(
            merged.iter().any(|s| mcp_server_name(s) == "other-http"),
            "different URL client server must survive"
        );
    }

    /// Sse shares the Http policy arms: a URL deny blocks an Sse definition; a non-matching one stays allowed.
    #[test]
    fn sse_transport_matches_http_policy_verdicts() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy,
        };

        let denied = acp::McpServer::Sse(
            acp::McpServerSse::new("corp-sse", "https://denied.corp.com/mcp").headers(vec![]),
        );
        let allowed = acp::McpServer::Sse(
            acp::McpServerSse::new("ok-sse", "https://ok.example/mcp").headers(vec![]),
        );
        let ms = settings_with_policy(McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Http {
                url_pattern: "https://denied.corp.com/*".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed_config.toml")),
        )));

        assert!(
            matches!(
                ms.mcp_verdict(&denied, foreign(denied.clone()).1),
                McpVerdict::Blocked(McpBlockReason::Deny { .. })
            ),
            "URL deny must bind the Sse transport"
        );

        let kept = apply_mcp_server_policy(
            vec![foreign(denied), foreign(allowed)],
            &std::collections::HashSet::new(),
            &ms,
        );
        let reason_of = |name: &str| {
            kept.iter()
                .find(|s| mcp_server_name(&s.server) == name)
                .expect("server present")
                .disabled_reason
                .clone()
        };
        assert!(
            matches!(reason_of("corp-sse"), Some(McpBlockReason::Deny { .. })),
            "merge must tag the denied Sse server"
        );
        assert!(
            reason_of("ok-sse").is_none(),
            "non-matching Sse server must stay allowed"
        );
    }

    /// The CLI verdict map reads disable-IGNORING discovery: a personally-disabled server must keep its deny.
    #[test]
    fn blocked_map_covers_personally_disabled_servers() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy,
        };

        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join(".grok")).unwrap();
        std::fs::write(
            cwd.path().join(".grok").join("config.toml"),
            r#"
disabled_mcp_servers = ["corp"]

[mcp_servers.corp]
url = "https://denied.corp.com/mcp"
"#,
        )
        .unwrap();

        let compat = distill_tools::types::compat::CompatConfig::default();
        let inputs = McpDiscoveryInputs {
            cwd: cwd.path(),
            plugin_registry: None,
            compat: &compat,
        };
        let discovered = discover_mcp_definitions_ignoring_disable(&inputs);
        assert!(
            discovered.contains_key("corp"),
            "disable-ignoring discovery must keep the disabled definition, got {:?}",
            discovered.keys().collect::<Vec<_>>()
        );

        let ms = settings_with_policy(McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Http {
                url_pattern: "https://denied.corp.com/*".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed_config.toml")),
        )));
        let blocked = mcp_blocked_reasons(
            discovered
                .iter()
                .map(|(name, (server, subject))| (name.as_str(), server, *subject)),
            &ms,
        );
        assert!(
            blocked.contains_key("corp"),
            "denied+disabled server must be in the verdict map"
        );
    }

    /// A repo-declared definition is discoverable while the folder is trusted and gone once it is not; user-tier and plugin-tier definitions survive either way.
    /// Runs in a re-exec of this binary: discovery reads `$GROK_HOME/config.toml` through the process-wide `distill_home()` `OnceLock`, so only a fresh process can isolate it.
    #[test]
    fn discovery_drops_project_definitions_for_untrusted_folder() {
        let distill_home = tempfile::tempdir().unwrap();
        std::fs::write(
            distill_home.path().join("config.toml"),
            format!("[mcp_servers.usersrv]\nurl = \"{USER_MCP_URL}\"\n"),
        )
        .unwrap();
        // module_path!() includes the crate name; libtest filters do not.
        let filter = module_path!()
            .split_once("::")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        let exe = std::env::current_exe().expect("current_exe");
        let mut cmd = std::process::Command::new(exe);
        cmd.env("GROK_HOME", distill_home.path())
            .env_remove("GROK_CONFIG")
            .env_remove("GROK_CONFIG_PATH")
            .env(UNTRUSTED_DISCOVERY_CHILD, "1")
            .arg("--ignored")
            .arg("--exact")
            .arg(format!("{filter}::untrusted_discovery_child"))
            .arg("--nocapture")
            .stdin(std::process::Stdio::null());
        distill_tty_utils::detach_std_command(&mut cmd);
        let output = cmd.output().expect("re-exec test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "isolated discovery child failed:\n{stdout}\n{stderr}"
        );
        // libtest exits 0 when an --exact filter matches nothing.
        assert!(
            stdout.contains(UNTRUSTED_DISCOVERY_PASS_MARK),
            "child did not run (filter matched nothing?)\n{stdout}\n{stderr}"
        );
    }

    const UNTRUSTED_DISCOVERY_CHILD: &str = "GROK_TEST_UNTRUSTED_DISCOVERY_CHILD";
    const UNTRUSTED_DISCOVERY_PASS_MARK: &str = "untrusted-discovery-child-passed";
    const USER_MCP_URL: &str = "https://user.example.com/mcp";
    const PLUGIN_MCP_URL: &str = "https://plugin.example.com/mcp";

    fn http_url(server: &acp::McpServer) -> Option<&str> {
        match server {
            acp::McpServer::Http(acp::McpServerHttp { url, .. }) => Some(url),
            _ => None,
        }
    }

    /// Body of `discovery_drops_project_definitions_for_untrusted_folder`; inert
    /// unless the parent armed it.
    #[test]
    #[ignore = "spawned as a subprocess by discovery_drops_project_definitions_for_untrusted_folder"]
    fn untrusted_discovery_child() {
        if std::env::var_os(UNTRUSTED_DISCOVERY_CHILD).is_none() {
            return;
        }
        let cwd = tempfile::tempdir().unwrap();
        git2::Repository::init(cwd.path()).unwrap();
        std::fs::create_dir_all(cwd.path().join(".grok")).unwrap();
        std::fs::write(
            cwd.path().join(".grok").join("config.toml"),
            r#"
[mcp_servers.projsrv]
command = "echo"
"#,
        )
        .unwrap();
        let plugin_root = tempfile::tempdir().unwrap();
        let registry =
            plugin_registry_with_inline_server(plugin_root.path(), "pluginsrv", PLUGIN_MCP_URL);

        // Keep the developer's ~/.claude.json and ~/.cursor/mcp.json out of discovery.
        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.claude.mcps = false;
        compat.cursor.mcps = false;
        let inputs = McpDiscoveryInputs {
            cwd: cwd.path(),
            plugin_registry: Some(&registry),
            compat: &compat,
        };

        crate::agent::folder_trust::record_for_test(cwd.path(), true);
        let trusted = discover_mcp_definitions_ignoring_disable(&inputs);
        let (_, subject) = trusted
            .get("projsrv")
            .expect("trusted repo-declared definition must be discoverable");
        assert!(
            subject.project_scoped,
            "repo-declared definition must be tagged project-scoped"
        );

        crate::agent::folder_trust::record_for_test(cwd.path(), false);
        let untrusted = discover_mcp_definitions_ignoring_disable(&inputs);
        assert!(
            !untrusted.contains_key("projsrv"),
            "untrusted repo-declared definition must not be discoverable, got {:?}",
            untrusted.keys().collect::<Vec<_>>()
        );
        let (user, _) = untrusted
            .get("usersrv")
            .expect("user-tier definition must survive the trust gate");
        assert_eq!(http_url(user), Some(USER_MCP_URL));
        let (plugin, _) = untrusted
            .get("pluginsrv")
            .expect("plugin-tier definition must survive the trust gate");
        assert_eq!(http_url(plugin), Some(PLUGIN_MCP_URL));

        println!("{UNTRUSTED_DISCOVERY_PASS_MARK}");
    }

    /// Pins the pool gate: binding deny drops; advisory binds vendor/
    /// project-claimed names but not TOML-owned (grok-native) definitions.
    #[test]
    fn agent_pool_filter_drops_policy_blocked_servers() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy, PolicySourceAuthority,
        };

        let corp = || {
            acp::McpServer::Http(
                acp::McpServerHttp::new("corp", "https://denied.corp.com/mcp").headers(vec![]),
            )
        };
        let deny = |authority: PolicySourceAuthority| {
            settings_with_policy(McpServerPolicy::single(
                McpServerAllowlist::new(
                    vec![],
                    vec![AllowedMcpServer::Http {
                        url_pattern: "https://denied.corp.com/*".into(),
                    }],
                    Some(std::path::PathBuf::from("/test/managed_config.toml")),
                )
                .with_authority(authority),
            ))
        };
        let names = |servers: &[acp::McpServer]| -> Vec<String> {
            servers
                .iter()
                .map(|s| mcp_server_name(s).to_string())
                .collect()
        };
        let none = std::collections::HashSet::new;
        let no_toml = HashMap::new;

        // A native deny binds everything: the matching server drops.
        let out = filter_policy_blocked_agent_mcp_with(
            vec![
                corp(),
                acp::McpServer::Http(
                    acp::McpServerHttp::new("ok", "https://ok.example/mcp").headers(vec![]),
                ),
            ],
            &no_toml(),
            &none(),
            &deny(PolicySourceAuthority::Native),
        );
        assert_eq!(names(&out), vec!["ok"]);

        // An advisory deny binds only foreign subjects: the TOML-owned
        // (grok-native) definition survives, the vendor-defined one drops.
        let toml_servers = HashMap::from([("corp".to_string(), corp())]);
        let advisory = deny(PolicySourceAuthority::Advisory);
        let kept =
            filter_policy_blocked_agent_mcp_with(vec![corp()], &toml_servers, &none(), &advisory);
        assert_eq!(names(&kept), vec!["corp"]);
        assert!(
            filter_policy_blocked_agent_mcp_with(vec![corp()], &no_toml(), &none(), &advisory)
                .is_empty()
        );

        // A project-claimed TOML name reclassifies foreign (fail closed).
        let project = std::collections::HashSet::from(["corp".to_string()]);
        assert!(
            filter_policy_blocked_agent_mcp_with(vec![corp()], &toml_servers, &project, &advisory)
                .is_empty()
        );

        // Squatting the TOML name with a different definition stays foreign under an advisory
        // name deny: another URL, a transport swap onto the Http name, a Stdio with another command.
        let stdio = |cmd: &str| {
            acp::McpServer::Stdio(acp::McpServerStdio::new(
                "corp",
                std::path::PathBuf::from(cmd),
            ))
        };
        let deny_name = settings_with_policy(McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![],
                vec![AllowedMcpServer::Name {
                    name: "corp".into(),
                }],
                Some(std::path::PathBuf::from("/test/managed_config.toml")),
            )
            .with_authority(PolicySourceAuthority::Advisory),
        ));
        let stdio_toml = HashMap::from([("corp".to_string(), stdio("good"))]);
        let http_squat = acp::McpServer::Http(
            acp::McpServerHttp::new("corp", "https://denied.corp.com/evil").headers(vec![]),
        );
        for (squatter, toml) in [
            (http_squat, &toml_servers),
            (stdio("evil"), &toml_servers),
            (stdio("evil"), &stdio_toml),
        ] {
            assert!(
                filter_policy_blocked_agent_mcp_with(vec![squatter], toml, &none(), &deny_name)
                    .is_empty()
            );
        }
        let kept = filter_policy_blocked_agent_mcp_with(
            vec![stdio("good")],
            &stdio_toml,
            &none(),
            &deny_name,
        );
        assert_eq!(names(&kept), vec!["corp"]);
    }

    /// The native match compares a `load_mcp_servers` payload with `load_mcp_servers_toml_only`;
    /// pin from disk that the two loaders agree, HashMap-ordered `env`/`headers` included.
    #[test]
    fn toml_loaders_agree_on_env_and_header_bearing_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        std::fs::create_dir_all(tmp.path().join(".grok")).unwrap();
        std::fs::write(
            tmp.path().join(".grok").join("config.toml"),
            r#"
[mcp_servers.parity_stdio]
command = "echo"
args = ["a", "b"]
env = { A = "1", B = "2", C = "3" }

[mcp_servers.parity_http]
url = "https://parity.example/mcp"
headers = { "X-A" = "1", "X-B" = "2", "X-C" = "3" }
"#,
        )
        .unwrap();

        let compat = distill_tools::types::compat::CompatConfig::default();
        let by_name = |servers: Vec<acp::McpServer>| -> HashMap<String, acp::McpServer> {
            servers
                .into_iter()
                .map(|s| (mcp_merge_key(&s), s))
                .collect()
        };
        let full = by_name(crate::util::config::load_mcp_servers(tmp.path(), &compat));
        let toml = by_name(crate::util::config::load_mcp_servers_toml_only(tmp.path()));
        for name in ["parity_stdio", "parity_http"] {
            let (a, b) = (full.get(name).unwrap(), toml.get(name).unwrap());
            assert!(mcp_server_definitions_equal(a, b), "{name}: {a:?} vs {b:?}");
        }
    }

    /// Two loads of the same TOML emit `env`/`headers` in HashMap order; the native match must not depend on it.
    #[test]
    fn definition_equality_ignores_env_and_header_order_but_not_args() {
        let stdio = |args: Vec<&str>, env: Vec<(&str, &str)>| {
            acp::McpServer::Stdio(
                acp::McpServerStdio::new("s", std::path::PathBuf::from("cmd"))
                    .args(args.into_iter().map(String::from).collect())
                    .env(
                        env.into_iter()
                            .map(|(k, v)| acp::EnvVariable::new(k, v))
                            .collect(),
                    ),
            )
        };
        let http = |headers: Vec<(&str, &str)>| {
            acp::McpServer::Http(
                acp::McpServerHttp::new("s", "https://x.example/mcp").headers(
                    headers
                        .into_iter()
                        .map(|(k, v)| acp::HttpHeader::new(k, v))
                        .collect(),
                ),
            )
        };

        assert!(mcp_server_definitions_equal(
            &stdio(vec!["a", "b"], vec![("A", "1"), ("B", "2")]),
            &stdio(vec!["a", "b"], vec![("B", "2"), ("A", "1")]),
        ));
        assert!(mcp_server_definitions_equal(
            &http(vec![("X-A", "1"), ("X-B", "2")]),
            &http(vec![("X-B", "2"), ("X-A", "1")]),
        ));
        assert!(!mcp_server_definitions_equal(
            &stdio(vec!["a", "b"], vec![]),
            &stdio(vec!["b", "a"], vec![]),
        ));
        assert!(!mcp_server_definitions_equal(
            &stdio(vec![], vec![("A", "1")]),
            &stdio(vec![], vec![("A", "2")]),
        ));
    }

    /// An advisory policy source binds only foreign-origin servers at the merge; a native source drops both.
    #[test]
    fn advisory_policy_exempts_grok_native_servers() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy, PolicySourceAuthority,
        };

        let server = || {
            acp::McpServer::Http(
                acp::McpServerHttp::new("corp", "https://denied.corp.com/mcp").headers(vec![]),
            )
        };
        let deny = |authority: PolicySourceAuthority| {
            McpServerPolicy::single(
                McpServerAllowlist::new(
                    vec![],
                    vec![AllowedMcpServer::Http {
                        url_pattern: "https://denied.corp.com/*".into(),
                    }],
                    Some(std::path::PathBuf::from("/test/managed-settings.json")),
                )
                .with_authority(authority),
            )
        };
        let subject = |origin: PolicySubjectOrigin| McpSubject {
            origin,
            project_scoped: false,
        };

        // Advisory deny + native server: survives.
        let tagged = apply_mcp_server_policy(
            vec![(server(), subject(PolicySubjectOrigin::GrokNative))],
            &std::collections::HashSet::new(),
            &settings_with_policy(deny(PolicySourceAuthority::Advisory)),
        );
        assert!(
            at(&tagged, 0).disabled_reason.is_none(),
            "advisory deny must not bind a grok-native server"
        );

        // Advisory deny + foreign server: binds.
        let tagged = apply_mcp_server_policy(
            vec![(server(), subject(PolicySubjectOrigin::Foreign))],
            &std::collections::HashSet::new(),
            &settings_with_policy(deny(PolicySourceAuthority::Advisory)),
        );
        assert!(
            at(&tagged, 0).disabled_reason.is_some(),
            "advisory deny must bind a foreign server"
        );

        // Native (TOML) deny binds the native server too.
        let tagged = apply_mcp_server_policy(
            vec![(server(), subject(PolicySubjectOrigin::GrokNative))],
            &std::collections::HashSet::new(),
            &settings_with_policy(deny(PolicySourceAuthority::Native)),
        );
        assert!(
            at(&tagged, 0).disabled_reason.is_some(),
            "a native policy source binds every origin"
        );
    }

    /// Single-plugin registry declaring one inline HTTP MCP server — the simplest injectable
    /// grok-native definition (a test must not touch the process-global grok home).
    fn plugin_registry_with_inline_server(
        plugin_root: &std::path::Path,
        server_name: &str,
        url: &str,
    ) -> distill_agent::plugins::PluginRegistry {
        use distill_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
        use distill_agent::plugins::manifest::{PathOrInline, PluginManifest};
        use distill_agent::plugins::{PluginRegistry, PluginScope};

        std::fs::create_dir_all(plugin_root).unwrap();
        let manifest = PluginManifest {
            name: "native-plugin".into(),
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            skills: None,
            commands: None,
            agents: None,
            hooks: None,
            mcp_servers: Some(PathOrInline::Inline(serde_json::json!({
                server_name: { "type": "http", "url": url }
            }))),
            lsp_servers: None,
        };
        let id = PluginId::new(PluginScope::User, plugin_root, "native-plugin");
        let dp = DiscoveredPlugin {
            manifest,
            id,
            root: plugin_root.to_path_buf(),
            canonical_root: plugin_root.to_path_buf(),
            scope: PluginScope::User,
            origin: distill_agent::plugins::PluginOrigin::UserGrok,
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs: vec![],
            hooks_path: None,
            mcp_config_path: None,
            lsp_config_path: None,
            conflict: None,
        };
        PluginRegistry::from_discovered(vec![dp], &[], &["native-plugin".to_string()])
    }

    /// A client re-forwarding a native-defined name keeps the native subject; a client-only name fails closed to foreign.
    #[test]
    fn client_forwarded_native_name_keeps_native_subject() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy, PolicySourceAuthority,
        };

        let tmp = tempfile::tempdir().unwrap();
        let registry = plugin_registry_with_inline_server(
            &tmp.path().join("native-plugin"),
            "corp",
            "https://denied.corp.com/mcp",
        );
        let cwd = empty_cwd();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let ms = settings_with_policy(McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![],
                vec![AllowedMcpServer::Http {
                    url_pattern: "https://denied.corp.com/*".into(),
                }],
                Some(std::path::PathBuf::from("/test/managed-settings.json")),
            )
            .with_authority(PolicySourceAuthority::Advisory),
        ));

        let client = |name: &str| {
            acp::McpServer::Http(
                acp::McpServerHttp::new(name, "https://denied.corp.com/mcp").headers(vec![]),
            )
        };
        let merged = merge_managed_mcp_servers_with_policy_from(
            vec![client("corp"), client("rogue")],
            cwd.path(),
            Some(&registry),
            &compat,
            &ms,
        );
        let reason_of = |name: &str| {
            merged
                .iter()
                .find(|s| mcp_server_name(&s.server) == name)
                .unwrap_or_else(|| panic!("{name} must be in the merge"))
                .disabled_reason
                .as_ref()
        };
        assert!(
            reason_of("corp").is_none(),
            "advisory deny must not bind the client-forwarded copy of a native server"
        );
        assert!(
            reason_of("rogue").is_some(),
            "client-only server must stay foreign and be tagged"
        );
    }

    /// Inheritance requires the SAME definition: a native name on a different endpoint falls back to foreign.
    #[test]
    fn client_redefinition_of_native_name_falls_back_to_foreign() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy, PolicySourceAuthority,
        };

        let tmp = tempfile::tempdir().unwrap();
        let registry = plugin_registry_with_inline_server(
            &tmp.path().join("native-plugin"),
            "corp",
            "https://ok.example.com/mcp",
        );
        let cwd = empty_cwd();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let ms = settings_with_policy(McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![],
                vec![AllowedMcpServer::Http {
                    url_pattern: "https://evil.example.com/*".into(),
                }],
                Some(std::path::PathBuf::from("/test/managed-settings.json")),
            )
            .with_authority(PolicySourceAuthority::Advisory),
        ));

        // Same native name, denied endpoint.
        let client = acp::McpServer::Http(
            acp::McpServerHttp::new("corp", "https://evil.example.com/mcp").headers(vec![]),
        );
        let merged = merge_managed_mcp_servers_with_policy_from(
            vec![client],
            cwd.path(),
            Some(&registry),
            &compat,
            &ms,
        );
        let corp = merged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "corp")
            .expect("corp must be in the merge");
        assert!(
            corp.disabled_reason.is_some(),
            "a client redefinition of a native name must stay foreign and be tagged"
        );
    }

    /// Origin table: only config.toml and plugin servers are grok-native; a project-scoped name reclassifies foreign.
    #[test]
    fn mcp_subject_classifies_sources() {
        use distill_tools::types::config_source::ConfigSource;

        let server = acp::McpServer::Http(
            acp::McpServerHttp::new("srv", "https://s.example.com/mcp").headers(vec![]),
        );
        let empty = std::collections::HashSet::new();
        let native = [
            ConfigSource::ConfigToml {
                path: "/u/.grok/config.toml".into(),
            },
            ConfigSource::Plugin {
                plugin_name: "p".into(),
                path: "/p".into(),
            },
        ];
        for source in &native {
            assert_eq!(
                mcp_subject(&server, source, &empty).origin,
                PolicySubjectOrigin::GrokNative,
                "{source:?}"
            );
        }
        let foreign = [
            ConfigSource::Project {
                path: "/repo/.grok".into(),
            },
            ConfigSource::User { path: "/u".into() },
            ConfigSource::Bundled { path: "/b".into() },
            ConfigSource::Server { path: "/s".into() },
            ConfigSource::ClaudeJson {
                path: "/u/.claude.json".into(),
            },
            ConfigSource::McpJson {
                path: "/repo/.mcp.json".into(),
            },
            ConfigSource::Cli {
                path: "/cli".into(),
            },
            ConfigSource::Managed { path: None },
            ConfigSource::Builtin,
        ];
        for source in &foreign {
            assert_eq!(
                mcp_subject(&server, source, &empty).origin,
                PolicySubjectOrigin::Foreign,
                "{source:?}"
            );
        }
        // A project-scoped name strips the native classification.
        let project: std::collections::HashSet<String> = ["srv".to_string()].into();
        assert_eq!(
            mcp_subject(&server, at(&native, 0), &project).origin,
            PolicySubjectOrigin::Foreign
        );
    }

    /// End-to-end merge glue with injected settings: the lockdown tags unlisted project servers and the pin drops project MCP.
    #[test]
    fn injected_settings_drive_lockdown_and_project_pin_through_merge() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, ManagedSettings, McpServerAllowlist, McpServerPolicy,
            PolicyLayerOwnership, PolicyPin,
        };

        let cwd = empty_cwd();
        std::fs::create_dir_all(cwd.path().join(".cursor")).unwrap();
        std::fs::write(
            cwd.path().join(".cursor").join("mcp.json"),
            r#"{"mcpServers": {
                "granted": {"url": "https://ok.example.com/mcp"},
                "ungranted": {"url": "https://other.example.com/mcp"}
            }}"#,
        )
        .unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let merge = |ms: &ManagedSettings| {
            let merged =
                merge_managed_mcp_servers_with_policy_from(vec![], cwd.path(), None, &compat, ms);
            merged
                .iter()
                .map(|s| {
                    (
                        mcp_server_name(&s.server).to_string(),
                        s.disabled_reason.is_some(),
                    )
                })
                .collect::<HashMap<String, bool>>()
        };

        // No policy: both project servers load untagged.
        let mut ms = ManagedSettings::default();
        let by_name = merge(&ms);
        assert_eq!(by_name.get("granted"), Some(&false));
        assert_eq!(by_name.get("ungranted"), Some(&false));

        // Managed-only lockdown with one grant, tagged through the real
        // origins-map handoff; the /etc/grok layer makes both Admin-owned.
        ms.mcp_allowlist = McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![AllowedMcpServer::Http {
                    url_pattern: "https://ok.example.com/*".into(),
                }],
                vec![],
                Some(std::path::PathBuf::from("/etc/grok/managed_config.toml")),
            )
            .with_managed_only()
            .with_ownership(PolicyLayerOwnership::Admin),
        );
        let by_name = merge(&ms);
        assert_eq!(by_name.get("granted"), Some(&false));
        assert_eq!(
            by_name.get("ungranted"),
            Some(&true),
            "unlisted server must be tagged blocked under managed-only"
        );

        // Project pin: ungranted project MCP is dropped outright; the
        // allow-granted server survives the pin (admin grant, admin pin).
        ms.project_mcp = PolicyPin::Disabled {
            source: std::path::PathBuf::from("/etc/grok/managed_config.toml"),
            ownership: PolicyLayerOwnership::Admin,
        };
        let by_name = merge(&ms);
        assert_eq!(
            by_name.get("granted"),
            Some(&false),
            "allow-granted project server survives the pin"
        );
        assert!(
            !by_name.contains_key("ungranted"),
            "pinned-off project server must be dropped"
        );
    }

    /// The merge drops a `deniedMcpServers` match and classifies it `Denylist`, not a missing allow entry.
    #[test]
    fn merge_drops_denied_server_and_classifies_as_denylist() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy,
        };

        // Deny-only policy (no allowlist) blocking one host.
        let allowlist = McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Http {
                url_pattern: "https://blocked.corp.com/*".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed-settings.json")),
        ));

        let tagged = apply_mcp_server_policy(
            vec![
                foreign(acp::McpServer::Http(
                    acp::McpServerHttp::new("blocked", "https://blocked.corp.com/mcp")
                        .headers(vec![]),
                )),
                foreign(acp::McpServer::Http(
                    acp::McpServerHttp::new("ok", "https://ok.corp.com/mcp").headers(vec![]),
                )),
            ],
            &std::collections::HashSet::new(),
            &settings_with_policy(allowlist),
        );

        // Denied server is classified as a denylist hit, not a missing-allow.
        let blocked = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "blocked")
            .expect("denied server present in policy output");
        assert!(
            matches!(blocked.disabled_reason, Some(McpBlockReason::Deny { .. })),
            "expected Deny reason, got {:?}",
            blocked.disabled_reason
        );

        // Non-denied server passes untouched.
        let ok = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "ok")
            .expect("allowed server present in policy output");
        assert!(ok.disabled_reason.is_none());

        // The public `merge_managed_mcp_servers` drop predicate removes exactly the denied server
        let surviving: Vec<&str> = tagged
            .iter()
            .filter(|s| s.disabled_reason.is_none())
            .map(|s| mcp_server_name(&s.server))
            .collect();
        assert_eq!(
            surviving,
            ["ok"],
            "denied server must be dropped by the merge"
        );
    }

    /// The documented admin signal must land in the always-on unified.jsonl, not just tracing.
    #[test]
    fn policy_block_writes_unified_log_warn() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy,
        };

        let allowlist = McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Name {
                name: "corp-denied-unified-log".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed-settings.json")),
        ));
        let tagged = apply_mcp_server_policy(
            vec![foreign(acp::McpServer::Http(
                acp::McpServerHttp::new("corp-denied-unified-log", "https://denied.example/mcp")
                    .headers(vec![]),
            ))],
            &std::collections::HashSet::new(),
            &settings_with_policy(allowlist),
        );
        assert!(
            at(&tagged, 0).disabled_reason.is_some(),
            "server must be tagged"
        );

        let log = distill_telemetry::unified_log::snapshot_log().unwrap_or_default();
        let log = String::from_utf8_lossy(&log);
        assert!(
            log.lines().any(
                |l| l.contains("MCP server blocked by managed settings policy")
                    && l.contains("corp-denied-unified-log")
            ),
            "block signal missing from unified log"
        );
    }

    /// A bare policy `serverName` deny drops the managed (prefixed) server as a
    /// `Denylist` hit, exact-match only (no substring over-match).
    #[test]
    fn merge_drops_server_denied_by_name_including_managed_prefix() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy,
        };

        let allowlist = McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Name {
                name: "slack".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed-settings.json")),
        ));

        let tagged = apply_mcp_server_policy(
            vec![
                foreign(acp::McpServer::Http(
                    acp::McpServerHttp::new("grok_com_slack", "https://mcp.slack.com/sse")
                        .headers(vec![]),
                )),
                // Substring-only match must not be denied.
                foreign(acp::McpServer::Http(
                    acp::McpServerHttp::new("slackbot", "https://slackbot.example.com/mcp")
                        .headers(vec![]),
                )),
            ],
            &std::collections::HashSet::new(),
            &settings_with_policy(allowlist),
        );

        let slack = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "grok_com_slack")
            .expect("managed server present in policy output");
        assert!(
            matches!(slack.disabled_reason, Some(McpBlockReason::Deny { .. })),
            "name-denied managed server must classify as Deny, got {:?}",
            slack.disabled_reason
        );

        let bot = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "slackbot")
            .expect("unrelated server present in policy output");
        assert!(
            bot.disabled_reason.is_none(),
            "substring-only match must not be denied by name"
        );

        let surviving: Vec<&str> = tagged
            .iter()
            .filter(|s| s.disabled_reason.is_none())
            .map(|s| mcp_server_name(&s.server))
            .collect();
        assert_eq!(surviving, ["slackbot"]);
    }

    /// Managed-only at the merge chokepoint: allowlisted argv/URL servers
    /// survive; everything else drops as an allowlist miss.
    #[test]
    fn managed_only_lockdown_drops_non_allowlisted_keeps_allowlisted() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy,
        };

        let policy = McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![
                    AllowedMcpServer::StdioArgv {
                        argv: vec![
                            "npx".into(),
                            "@example-corp/ui-kit-mcp".into(),
                            "enterprise-webc".into(),
                        ],
                    },
                    AllowedMcpServer::Http {
                        url_pattern: "https://mcp.figma.com/*".into(),
                    },
                ],
                vec![],
                Some(std::path::PathBuf::from("/etc/grok/requirements.toml")),
            )
            .with_managed_only(),
        );

        let ui_kit = acp::McpServer::Stdio(
            acp::McpServerStdio::new("ui-kit", std::path::PathBuf::from("npx")).args(vec![
                "@example-corp/ui-kit-mcp".into(),
                "enterprise-webc".into(),
            ]),
        );
        let rogue_stdio = acp::McpServer::Stdio(acp::McpServerStdio::new(
            "rogue",
            std::path::PathBuf::from("python3"),
        ));
        let figma = acp::McpServer::Http(
            acp::McpServerHttp::new("figma", "https://mcp.figma.com/mcp").headers(vec![]),
        );
        let rogue_http = acp::McpServer::Http(
            acp::McpServerHttp::new("rogue-http", "https://evil.example.com/mcp").headers(vec![]),
        );

        let tagged = apply_mcp_server_policy(
            vec![
                foreign(ui_kit),
                foreign(rogue_stdio),
                foreign(figma),
                foreign(rogue_http),
            ],
            &std::collections::HashSet::new(),
            &settings_with_policy(policy),
        );
        let surviving: Vec<&str> = tagged
            .iter()
            .filter(|s| s.disabled_reason.is_none())
            .map(|s| mcp_server_name(&s.server))
            .collect();
        assert_eq!(surviving, ["ui-kit", "figma"]);

        let rogue = tagged
            .iter()
            .find(|s| mcp_server_name(&s.server) == "rogue")
            .expect("rogue stdio present in policy output");
        assert!(
            matches!(
                rogue.disabled_reason,
                Some(McpBlockReason::NotGranted { .. })
            ),
            "lockdown drop must classify as allowlist miss, got {:?}",
            rogue.disabled_reason
        );
    }

    /// Project-MCP pin: project servers drop unless allowlisted; non-project
    /// servers and an unpinned policy are unaffected.
    #[test]
    fn project_mcp_pin_drops_project_servers_unless_allowlisted() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, McpServerAllowlist, McpServerPolicy, PolicyLayerOwnership, PolicyPin,
        };

        let servers = || {
            vec![
                acp::McpServer::Http(
                    acp::McpServerHttp::new("projsrv", "https://proj.example.com/mcp")
                        .headers(vec![]),
                ),
                acp::McpServer::Http(
                    acp::McpServerHttp::new("projallowed", "https://allowed.example.com/mcp")
                        .headers(vec![]),
                ),
                acp::McpServer::Http(
                    acp::McpServerHttp::new("usersrv", "https://user.example.com/mcp")
                        .headers(vec![]),
                ),
            ]
        };
        let project_names: std::collections::HashSet<String> =
            ["projsrv".to_string(), "projallowed".to_string()]
                .into_iter()
                .collect();
        // The /etc/grok layer is admin-owned; the pin below carries the same
        // ownership, so the exception grant must be admin-owned too.
        let policy = McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![AllowedMcpServer::Http {
                    url_pattern: "https://allowed.example.com/*".into(),
                }],
                vec![],
                Some(std::path::PathBuf::from("/etc/grok/requirements.toml")),
            )
            .with_ownership(PolicyLayerOwnership::Admin),
        );
        let subject = |name: &str| McpSubject {
            origin: PolicySubjectOrigin::Foreign,
            project_scoped: project_names.contains(name),
        };
        let paired = || -> Vec<(acp::McpServer, McpSubject)> {
            servers()
                .into_iter()
                .map(|s| {
                    let subject = subject(mcp_server_name(&s));
                    (s, subject)
                })
                .collect()
        };

        // Pin disabled: project servers dropped, except the explicitly
        // allowlisted one; the user-tier server survives.
        let mut ms = settings_with_policy(policy.clone());
        ms.project_mcp = PolicyPin::Disabled {
            source: std::path::PathBuf::from("/etc/grok/requirements.toml"),
            ownership: PolicyLayerOwnership::Admin,
        };
        let kept = apply_mcp_server_policy(paired(), &std::collections::HashSet::new(), &ms);
        let names: Vec<&str> = kept.iter().map(|s| mcp_server_name(&s.server)).collect();
        assert_eq!(names, ["projallowed", "usersrv"]);

        // The verdict API's pin leg classifies the drop as a ProjectPin hit naming the pinning layer.
        let listed = servers();
        let reason = ms
            .mcp_project_pin_block(at(&listed, 0), subject("projsrv"))
            .expect("project server without a grant is dropped");
        assert!(
            matches!(&reason, McpBlockReason::ProjectPin { source }
                if source == std::path::Path::new("/etc/grok/requirements.toml")),
            "got {reason:?}"
        );
        assert!(
            ms.mcp_project_pin_block(at(&listed, 1), subject("projallowed"))
                .is_none(),
            "allowlisted project server is not dropped"
        );

        // Unpinned: everything passes through.
        let kept = apply_mcp_server_policy(
            paired(),
            &std::collections::HashSet::new(),
            &settings_with_policy(policy),
        );
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn lower_precedence_http_servers_are_blocked_by_toml_name_claims() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join(".grok")).unwrap();
        std::fs::write(
            cwd.path().join(".grok").join("config.toml"),
            r#"
[mcp_servers.github]
url = "https://config.example.com/mcp"
enabled = false
"#,
        )
        .unwrap();
        git2::Repository::init(cwd.path()).unwrap();
        std::fs::write(
            cwd.path().join(".mcp.json"),
            r#"{
                "mcpServers": {
                    "github": {
                        "url": "https://json.example.com/mcp"
                    }
                }
            }"#,
        )
        .unwrap();

        let compat = distill_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(vec![], cwd.path(), None, &compat);
        assert!(
            !merged.iter().any(|server| matches!(
                server,
                acp::McpServer::Http(acp::McpServerHttp { name, .. }) if name == "github"
            )),
            "config.toml should block same-named lower-precedence HTTP servers"
        );
    }

    /// Builds a trusted git repo whose project config.toml declares two HTTP servers sharing one URL, each with its own auth header.
    /// This mirrors a real setup: one ClickHouse endpoint, two orgs.
    fn same_url_project_repo() -> tempfile::TempDir {
        let cwd = empty_cwd();
        std::fs::create_dir_all(cwd.path().join(".grok")).unwrap();
        std::fs::write(
            cwd.path().join(".grok").join("config.toml"),
            r#"
[mcp_servers.gb5207-org1]
url = "https://dup-url.example.test/mcp"

[mcp_servers.gb5207-org1.headers]
Authorization = "Bearer org1-token"

[mcp_servers.gb5207-org2]
url = "https://dup-url.example.test/mcp"

[mcp_servers.gb5207-org2.headers]
Authorization = "Bearer org2-token"
"#,
        )
        .unwrap();
        git2::Repository::init(cwd.path()).unwrap();
        crate::agent::folder_trust::record_for_test(cwd.path(), true);
        cwd
    }

    /// Server NAME is the identity: two entries sharing one URL are distinct servers, and each keeps its own transport config.
    #[test]
    fn same_url_different_names_both_survive_merge() {
        let cwd = same_url_project_repo();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let merged = merge_managed_mcp_servers(vec![], cwd.path(), None, &compat);

        let auth_header = |name: &str| -> &str {
            let server = merged
                .iter()
                .find(|s| mcp_server_name(s) == name)
                .unwrap_or_else(|| panic!("{name} must survive the merge"));
            match server {
                acp::McpServer::Http(acp::McpServerHttp { headers, .. }) => headers
                    .iter()
                    .find(|h| h.name == "Authorization")
                    .unwrap_or_else(|| panic!("{name} must keep its Authorization header"))
                    .value
                    .as_str(),
                other => panic!("expected Http server, got {other:?}"),
            }
        };
        assert_eq!(auth_header("gb5207-org1"), "Bearer org1-token");
        assert_eq!(auth_header("gb5207-org2"), "Bearer org2-token");
    }

    #[test]
    fn same_url_different_names_both_sourced_from_toml() {
        use distill_tools::types::config_source::ConfigSource;

        let cwd = same_url_project_repo();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let sourced = merge_managed_mcp_servers_sourced(cwd.path(), None, &compat);

        for name in ["gb5207-org1", "gb5207-org2"] {
            let (_, source) = sourced
                .iter()
                .find(|(s, _)| mcp_server_name(s) == name)
                .unwrap_or_else(|| panic!("{name} must be in the sourced merge"));
            assert!(
                matches!(source, ConfigSource::ConfigToml { .. }),
                "{name} must be attributed to config.toml, got {source:?}"
            );
        }
    }

    /// End-to-end folder-trust gate through the public merge: an untrusted workspace's project `.mcp.json` server is dropped before spawn.
    /// A client-supplied server still survives, and a trusted workspace keeps its `.mcp.json` server.
    #[test]
    fn untrusted_workspace_drops_project_mcp_servers() {
        fn repo_with_project_server() -> tempfile::TempDir {
            let cwd = tempfile::tempdir().unwrap();
            git2::Repository::init(cwd.path()).unwrap();
            std::fs::write(
                cwd.path().join(".mcp.json"),
                r#"{"mcpServers": {"projsrv": {"url": "https://proj.example.com/mcp"}}}"#,
            )
            .unwrap();
            cwd
        }
        let compat = distill_tools::types::compat::CompatConfig::default();

        let untrusted = repo_with_project_server();
        crate::agent::folder_trust::record_for_test(untrusted.path(), false);
        let client = vec![acp::McpServer::Http(
            acp::McpServerHttp::new(
                "clientsrv".to_string(),
                "https://client.example.com/mcp".to_string(),
            )
            .headers(vec![]),
        )];
        let merged = merge_managed_mcp_servers(client, untrusted.path(), None, &compat);
        assert!(
            !merged.iter().any(|s| mcp_server_name(s) == "projsrv"),
            "untrusted workspace must drop its repo-local MCP server"
        );
        assert!(
            merged.iter().any(|s| mcp_server_name(s) == "clientsrv"),
            "client-supplied server must be retained when untrusted"
        );

        let trusted = repo_with_project_server();
        crate::agent::folder_trust::record_for_test(trusted.path(), true);
        let merged = merge_managed_mcp_servers(vec![], trusted.path(), None, &compat);
        assert!(
            merged.iter().any(|s| mcp_server_name(s) == "projsrv"),
            "trusted workspace must keep its repo-local MCP server"
        );
    }

    /// A project-pin-blocked definition must stay discoverable, or toggle/doctor/list misreport "not found".
    #[test]
    fn discovery_keeps_pin_blocked_project_definition_for_verdicts() {
        use distill_workspace::permission::resolution::{PolicyLayerOwnership, PolicyPin};

        let cwd = empty_cwd();
        write_cursor_project_mcp(cwd.path(), "projsrv");
        let compat = distill_tools::types::compat::CompatConfig::default();
        let inputs = McpDiscoveryInputs {
            cwd: cwd.path(),
            plugin_registry: None,
            compat: &compat,
        };
        let discovered = discover_mcp_definitions_ignoring_disable(&inputs);
        let (server, subject) = discovered
            .get("projsrv")
            .expect("project definition must stay discoverable under a pin");
        assert!(subject.project_scoped);

        let mut ms = distill_workspace::permission::resolution::ManagedSettings::default();
        ms.project_mcp = PolicyPin::Disabled {
            source: std::path::PathBuf::from("/etc/grok/requirements.toml"),
            ownership: PolicyLayerOwnership::Admin,
        };
        match ms.mcp_verdict(server, *subject) {
            McpVerdict::Blocked(McpBlockReason::ProjectPin { source }) => {
                assert_eq!(
                    source,
                    std::path::PathBuf::from("/etc/grok/requirements.toml")
                );
            }
            other => panic!("expected ProjectPin refusal, got {other:?}"),
        }
    }

    #[test]
    fn load_plugin_mcp_creates_stdio_server_with_env_substitution() {
        let config: crate::util::config::McpConfig = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "echo-mcp": {
                    "command": "python3",
                    "args": ["${GROK_PLUGIN_ROOT}/mcp-echo-server.py"]
                }
            }
        }))
        .expect("parse test MCP config");

        let (servers, _) = load_plugin_mcp_servers_from_config(
            &config,
            "team-tool",
            "/home/user/.grok/plugins/team-tool",
            "/home/user/.grok/plugin-data/team-tool",
        );

        assert_eq!(servers.len(), 1, "should create one server");
        match &at(&servers, 0) {
            acp::McpServer::Stdio(acp::McpServerStdio {
                name,
                command,
                args,
                ..
            }) => {
                assert_eq!(name, "echo-mcp");
                assert_eq!(command.display().to_string(), "python3");
                assert_eq!(
                    args.as_slice(),
                    &["/home/user/.grok/plugins/team-tool/mcp-echo-server.py"]
                );
            }
            _other => panic!("expected Stdio server"),
        }
    }

    /// A sticky `enabled = false` disable must drop a plugin server through the REAL merge, not a hand-built set.
    #[test]
    fn plugin_mcp_disabled_server_excluded_from_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = plugin_registry_with_inline_server(
            &tmp.path().join("plug"),
            "plugsrv",
            "https://plug.example.test/mcp",
        );
        let cwd = empty_cwd();
        let compat = distill_tools::types::compat::CompatConfig::default();

        let merged = merge_managed_mcp_servers(vec![], cwd.path(), Some(&registry), &compat);
        assert!(
            merged.iter().any(|s| mcp_server_name(s) == "plugsrv"),
            "plugin server must merge before the disable"
        );

        git2::Repository::init(cwd.path()).unwrap();
        std::fs::create_dir_all(cwd.path().join(".grok")).unwrap();
        std::fs::write(
            cwd.path().join(".grok").join("config.toml"),
            "[mcp_servers.plugsrv]\nurl = \"https://plug.example.test/mcp\"\nenabled = false\n",
        )
        .unwrap();
        let merged = merge_managed_mcp_servers(vec![], cwd.path(), Some(&registry), &compat);
        assert!(
            !merged.iter().any(|s| mcp_server_name(s) == "plugsrv"),
            "disabled plugin server must be dropped by the merge"
        );
    }

    #[test]
    fn load_plugin_mcp_from_value_accepts_direct_map() {
        let value = serde_json::json!({
            "sentry": { "type": "http", "url": "https://mcp.sentry.dev/mcp" }
        });
        let (servers, _) =
            load_plugin_mcp_servers_from_value(&value, "sentry", "/tmp/p", "/tmp/pd");
        assert_eq!(servers.len(), 1);
        match &at(&servers, 0) {
            acp::McpServer::Http(acp::McpServerHttp { name, url, .. }) => {
                assert_eq!(name, "sentry");
                assert_eq!(url, "https://mcp.sentry.dev/mcp");
            }
            _other => panic!("expected Http server"),
        }
    }

    #[test]
    fn plugin_server_deduped_across_file_and_inline() {
        use distill_agent::plugins::PluginRegistry;
        use distill_agent::plugins::PluginScope;
        use distill_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
        use distill_agent::plugins::manifest::{PathOrInline, PluginManifest};

        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("sentry");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let mcp_json = plugin_root.join(".mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"sentry":{"type":"http","url":"https://mcp.sentry.dev/mcp"}}}"#,
        )
        .unwrap();

        let manifest = PluginManifest {
            name: "sentry".into(),
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            skills: None,
            commands: None,
            agents: None,
            hooks: None,
            mcp_servers: Some(PathOrInline::Inline(serde_json::json!({
                "sentry": { "type": "http", "url": "https://mcp.sentry.dev/mcp" }
            }))),
            lsp_servers: None,
        };
        let id = PluginId::new(PluginScope::User, &plugin_root, "sentry");
        let dp = DiscoveredPlugin {
            manifest,
            id,
            root: plugin_root.clone(),
            canonical_root: plugin_root.clone(),
            scope: PluginScope::User,
            origin: distill_agent::plugins::PluginOrigin::UserGrok,
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs: vec![],
            hooks_path: None,
            mcp_config_path: Some(mcp_json),
            lsp_config_path: None,
            conflict: None,
        };
        let registry = PluginRegistry::from_discovered(vec![dp], &[], &["sentry".to_string()]);

        let cwd = tempfile::tempdir().unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let sourced = merge_managed_mcp_servers_sourced(cwd.path(), Some(&registry), &compat);

        let sentry_count = sourced
            .iter()
            .filter(|(s, _)| mcp_server_name(s) == "sentry")
            .count();
        assert_eq!(
            sentry_count, 1,
            "sentry declared in both .mcp.json and inline must register exactly once"
        );
    }

    #[test]
    fn plugin_same_name_different_url_keeps_file_server() {
        use distill_agent::plugins::PluginRegistry;
        use distill_agent::plugins::PluginScope;
        use distill_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
        use distill_agent::plugins::manifest::{PathOrInline, PluginManifest};

        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("sentry");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let mcp_json = plugin_root.join(".mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"sentry":{"type":"http","url":"https://file.example/mcp"}}}"#,
        )
        .unwrap();

        let manifest = PluginManifest {
            name: "sentry".into(),
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            skills: None,
            commands: None,
            agents: None,
            hooks: None,
            mcp_servers: Some(PathOrInline::Inline(serde_json::json!({
                "sentry": { "type": "http", "url": "https://inline.example/mcp" }
            }))),
            lsp_servers: None,
        };
        let id = PluginId::new(PluginScope::User, &plugin_root, "sentry");
        let dp = DiscoveredPlugin {
            manifest,
            id,
            root: plugin_root.clone(),
            canonical_root: plugin_root.clone(),
            scope: PluginScope::User,
            origin: distill_agent::plugins::PluginOrigin::UserGrok,
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs: vec![],
            hooks_path: None,
            mcp_config_path: Some(mcp_json),
            lsp_config_path: None,
            conflict: None,
        };
        let registry = PluginRegistry::from_discovered(vec![dp], &[], &["sentry".to_string()]);

        let cwd = tempfile::tempdir().unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let sourced = merge_managed_mcp_servers_sourced(cwd.path(), Some(&registry), &compat);

        let sentry: Vec<&acp::McpServer> = sourced
            .iter()
            .map(|(s, _)| s)
            .filter(|s| mcp_server_name(s) == "sentry")
            .collect();
        assert_eq!(
            sentry.len(),
            1,
            "same plugin declaring one server name twice must register exactly once"
        );
        match at(&sentry, 0) {
            acp::McpServer::Http(acp::McpServerHttp { url, .. }) => {
                assert_eq!(url, "https://file.example/mcp", "file source must win");
            }
            other => panic!("expected Http server, got {:?}", other),
        }
    }

    #[test]
    fn collect_plugin_oauth_configs_reads_byo_client_id_from_mcp_json() {
        use distill_agent::plugins::PluginRegistry;
        use distill_agent::plugins::PluginScope;
        use distill_agent::plugins::discovery::{DiscoveredPlugin, PluginId};
        use distill_agent::plugins::manifest::PluginManifest;

        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("slack");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let mcp_json = plugin_root.join(".mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"slack":{"type":"http","url":"https://mcp.slack.example/mcp","oauth":{"clientId":"slack-byo-client","callbackPort":3118}}}}"#,
        )
        .unwrap();

        let manifest = PluginManifest {
            name: "slack".into(),
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            skills: None,
            commands: None,
            agents: None,
            hooks: None,
            mcp_servers: None,
            lsp_servers: None,
        };
        let id = PluginId::new(PluginScope::User, &plugin_root, "slack");
        let dp = DiscoveredPlugin {
            manifest,
            id,
            root: plugin_root.clone(),
            canonical_root: plugin_root.clone(),
            scope: PluginScope::User,
            origin: distill_agent::plugins::PluginOrigin::UserGrok,
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs: vec![],
            hooks_path: None,
            mcp_config_path: Some(mcp_json),
            lsp_config_path: None,
            conflict: None,
        };
        let registry = PluginRegistry::from_discovered(vec![dp], &[], &["slack".to_string()]);

        let oauth = collect_plugin_oauth_configs(Some(&registry));
        assert_eq!(
            oauth
                .get("slack")
                .expect("slack oauth")
                .client_id
                .as_deref(),
            Some("slack-byo-client")
        );
    }

    #[test]
    fn collect_plugin_oauth_configs_none_registry_is_empty() {
        assert!(collect_plugin_oauth_configs(None).is_empty());
    }

    #[test]
    fn merge_plugin_oauth_respects_source_precedence() {
        use crate::util::config::{McpOAuthConfig, McpOAuthConfigMap};

        let byo = |id: &str| McpOAuthConfig {
            client_id: Some(id.to_string()),
            ..Default::default()
        };

        let mut base = McpOAuthConfigMap::new();
        base.insert("shared".to_string(), byo("file-client"));
        base.insert("toml-svc".to_string(), byo("toml-client"));

        let mut plugin = McpOAuthConfigMap::new();
        plugin.insert("shared".to_string(), byo("plugin-client"));
        plugin.insert("toml-svc".to_string(), byo("plugin-shadow"));
        plugin.insert("plugin-only".to_string(), byo("plugin-only-client"));

        let toml_names: std::collections::HashSet<String> =
            ["toml-svc".to_string()].into_iter().collect();
        merge_plugin_oauth_into(&mut base, plugin, &toml_names);

        assert_eq!(
            base.get("shared").unwrap().client_id.as_deref(),
            Some("plugin-client")
        );
        assert_eq!(
            base.get("toml-svc").unwrap().client_id.as_deref(),
            Some("toml-client")
        );
        assert_eq!(
            base.get("plugin-only").unwrap().client_id.as_deref(),
            Some("plugin-only-client")
        );
    }
}
