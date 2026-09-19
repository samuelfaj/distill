// Modified for Distill by Samuel Fajreldines, 2026.
//! `grok mcp doctor`: runtime health check for MCP servers.

use std::collections::HashMap;
use std::path::Path;

use distill_tools::types::config_source::ConfigSource;
use serde::Serialize;

use crate::session::mcp_servers;

// ── Report types ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ConfigSourceStatus {
    pub path: String,
    pub status: ConfigSourceState,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConfigSourceState {
    Found { server_count: usize },
    NotFound,
    Skipped { reason: String },
}

#[derive(Debug, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: String,
    pub target: String,
    pub source: String,
    pub checks: Vec<Check>,
    pub healthy: bool,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub label: String,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn pass(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            passed: true,
            detail: Some(detail.into()),
            hint: None,
        }
    }

    fn fail(label: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            passed: false,
            detail: Some(detail.into()),
            hint: Some(hint.into()),
        }
    }

    fn fail_no_hint(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            passed: false,
            detail: Some(detail.into()),
            hint: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub sources: Vec<ConfigSourceStatus>,
    pub servers: Vec<McpServerStatus>,
    #[serde(skip)]
    pub all_server_names: Vec<String>,
    pub healthy_count: usize,
    pub failing_count: usize,
}

// ── Server discovery ────────────────────────────────────────────

struct DiscoveredServer {
    server: agent_client_protocol::McpServer,
    source: ConfigSource,
}

/// Plugin registry as a one-shot CLI sees it: discovery gated by the live folder-trust verdict
/// (no session resolve has run for a one-shot command).
fn cli_plugin_registry(cwd: &Path) -> distill_agent::plugins::PluginRegistry {
    let trust_store = distill_agent::plugins::TrustStore::load();
    let mut plugins_cfg: crate::agent::config::PluginsConfig =
        crate::config::load_effective_config()
            .ok()
            .and_then(|t| t.get("plugins").and_then(|v| v.clone().try_into().ok()))
            .unwrap_or_default();
    plugins_cfg.merge_claude_enabled_plugins(Some(cwd));
    let mut plugin_config = plugins_cfg.to_discovery_config();
    let project_trusted = crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let discovered_plugins = distill_agent::plugins::discover_plugins(
        Some(cwd),
        &plugin_config,
        &trust_store,
        project_trusted,
    );
    plugin_config.populate_plugin_lists(&discovered_plugins);
    distill_agent::plugins::PluginRegistry::from_discovered(
        discovered_plugins,
        &plugin_config.disabled,
        &plugin_config.enabled,
    )
}

/// Which config file declares each TOML server: the user config.toml unless a
/// project layer (repo-root-first, nearest wins) redefines the name.
fn toml_declaring_paths(
    user_config: &Path,
    user_root: Option<&toml::Value>,
    project_layers: &[(std::path::PathBuf, toml::Value)],
) -> HashMap<String, std::path::PathBuf> {
    let mut declaring = HashMap::new();
    if let Some(root) = user_root {
        for name in crate::util::config::parse_mcp_servers_from_toml(root).into_keys() {
            declaring.insert(name, user_config.to_path_buf());
        }
    }
    for (path, root) in project_layers {
        for name in crate::util::config::parse_mcp_servers_from_toml(root).into_keys() {
            declaring.insert(name, path.clone());
        }
    }
    declaring
}

fn discover_servers(cwd: &Path) -> (Vec<ConfigSourceStatus>, Vec<DiscoveredServer>) {
    let plugin_registry = cli_plugin_registry(cwd);

    // mcp-doctor is a diagnostic tool; use default (all-on) compat to show everything.
    let sourced = crate::session::managed_mcp::merge_managed_mcp_servers_sourced(
        cwd,
        Some(&plugin_registry),
        &distill_tools::types::compat::CompatConfig::default(),
    );

    let distill_home = distill_tools::util::distill_home::distill_home();
    let user_config = distill_home.join("config.toml");
    let project_config_paths = crate::config::find_project_configs(cwd);
    let project_layers: Vec<(std::path::PathBuf, toml::Value)> = project_config_paths
        .iter()
        .filter_map(|p| {
            crate::config::load_config_file(p)
                .ok()
                .map(|root| (p.clone(), root))
        })
        .collect();
    let declaring = toml_declaring_paths(
        &user_config,
        crate::config::load_effective_config().ok().as_ref(),
        &project_layers,
    );

    let mut toml_counts: HashMap<std::path::PathBuf, usize> = HashMap::new();
    let mut claude_count = 0usize;
    let mut mcp_json_count = 0usize;
    let mut plugin_counts: HashMap<String, usize> = HashMap::new();
    let mut servers = Vec::new();
    for (server, source) in sourced {
        match &source {
            ConfigSource::ConfigToml { .. } | ConfigSource::Project { .. } => {
                let path = declaring
                    .get(mcp_servers::mcp_server_name(&server))
                    .cloned()
                    .unwrap_or_else(|| user_config.clone());
                *toml_counts.entry(path).or_default() += 1;
            }
            ConfigSource::ClaudeJson { .. } => claude_count += 1,
            ConfigSource::McpJson { .. } => mcp_json_count += 1,
            ConfigSource::Plugin { plugin_name, .. } => {
                *plugin_counts.entry(plugin_name.clone()).or_default() += 1;
            }
            _ => {}
        }
        servers.push(DiscoveredServer { server, source });
    }

    let mut sources = Vec::new();

    if user_config.is_file() {
        sources.push(ConfigSourceStatus {
            path: "~/.grok/config.toml".to_string(),
            status: ConfigSourceState::Found {
                server_count: toml_counts.get(&user_config).copied().unwrap_or(0),
            },
        });
    } else {
        sources.push(ConfigSourceStatus {
            path: "~/.grok/config.toml".to_string(),
            status: ConfigSourceState::NotFound,
        });
    }

    for config_path in &project_config_paths {
        if config_path.is_file() {
            sources.push(ConfigSourceStatus {
                path: config_path.display().to_string(),
                status: ConfigSourceState::Found {
                    server_count: toml_counts.get(config_path).copied().unwrap_or(0),
                },
            });
        }
    }

    for (name, count) in &plugin_counts {
        sources.push(ConfigSourceStatus {
            path: format!("plugin: {}", name),
            status: ConfigSourceState::Found {
                server_count: *count,
            },
        });
    }

    let claude_imported = crate::claude_import::is_claude_import_marked();
    if claude_imported {
        sources.push(ConfigSourceStatus {
            path: "~/.claude.json".to_string(),
            status: ConfigSourceState::Skipped {
                reason: "claude_compat imported = true".to_string(),
            },
        });
    } else if let Some(home) = distill_dirs::home_dir() {
        let claude_path = home.join(".claude.json");
        if claude_path.is_file() {
            sources.push(ConfigSourceStatus {
                path: "~/.claude.json".to_string(),
                status: ConfigSourceState::Found {
                    server_count: claude_count,
                },
            });
        } else {
            sources.push(ConfigSourceStatus {
                path: "~/.claude.json".to_string(),
                status: ConfigSourceState::NotFound,
            });
        }
    } else {
        sources.push(ConfigSourceStatus {
            path: "~/.claude.json".to_string(),
            status: ConfigSourceState::NotFound,
        });
    }

    if claude_imported {
        sources.push(ConfigSourceStatus {
            path: ".mcp.json".to_string(),
            status: ConfigSourceState::Skipped {
                reason: "claude_compat imported = true".to_string(),
            },
        });
    } else {
        let mcp_json_files = crate::util::config::find_mcp_json_files(cwd);
        if mcp_json_files.is_empty() {
            sources.push(ConfigSourceStatus {
                path: ".mcp.json".to_string(),
                status: ConfigSourceState::NotFound,
            });
        } else {
            sources.push(ConfigSourceStatus {
                path: ".mcp.json".to_string(),
                status: ConfigSourceState::Found {
                    server_count: mcp_json_count,
                },
            });
        }
    }

    (sources, servers)
}

// ── Check functions ─────────────────────────────────────────────

fn resolve_command(command: &str) -> Option<String> {
    let path = std::path::Path::new(command);
    if path.is_absolute() {
        return path.exists().then(|| command.to_string());
    }

    #[cfg(unix)]
    let which_cmd = "which";
    #[cfg(windows)]
    let which_cmd = "where";

    let mut cmd = std::process::Command::new(which_cmd);
    cmd.arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    distill_tools::util::detach_std_command(&mut cmd);
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
}

fn check_command_exists(command: &str) -> Check {
    match resolve_command(command) {
        Some(resolved) => Check::pass("command found", resolved),
        None => Check::fail(
            "command not found",
            command,
            "verify the binary exists and is in PATH",
        ),
    }
}

async fn check_server_start(
    acp_server: agent_client_protocol::McpServer,
    cwd: &Path,
) -> Result<(mcp_servers::McpClient, Check), Check> {
    let start = std::time::Instant::now();
    let noop = distill_session_events::EventWriter::noop();
    let ctx = mcp_servers::McpSpawnCtx::standalone(&noop)
        .with_oauth_discovery(mcp_servers::McpOauthDiscovery::Network);
    match mcp_servers::start_mcp_server(acp_server, Some(cwd), None, None, &ctx).await {
        Ok(client) => {
            let elapsed = start.elapsed();
            Ok((
                client,
                Check::pass("server started", format!("{:.1}s", elapsed.as_secs_f64())),
            ))
        }
        Err(e) => Err(format_mcp_error("server failed to start", &e)),
    }
}

async fn check_handshake(
    client: &mcp_servers::McpClient,
) -> Result<(mcp_servers::McpService, Check), Check> {
    match client.ensure_initialized().await {
        Ok(service) => {
            let protocol = service
                .peer_info()
                .map(|info| format!("protocol {}", info.protocol_version))
                .unwrap_or_else(|| "protocol unknown".to_string());
            Ok((service, Check::pass("handshake OK", protocol)))
        }
        Err(e) => Err(format_mcp_error("handshake failed", &e)),
    }
}

async fn check_tools_list(server_name: &str, service: &mcp_servers::McpService) -> Check {
    use distill_mcp::rmcp::model::PaginatedRequestParams;
    match service
        .list_tools(Some(PaginatedRequestParams::default()))
        .await
    {
        Ok(result) => tools_list_admission_check(
            server_name,
            result.tools.iter().map(|tool| tool.name.as_ref()),
        ),
        Err(e) => Check::fail("tools/list failed", e.to_string(), "check server logs"),
    }
}

/// Apply session admission to a `tools/list` snapshot so doctor matches the catalog.
fn tools_list_admission_check<'a>(
    server_name: &str,
    tool_names: impl IntoIterator<Item = &'a str>,
) -> Check {
    let mut listed = 0;
    let mut skipped = Vec::new();
    for name in tool_names {
        listed += 1;
        if let Err(reason) = mcp_servers::qualify_mcp_tool_name(server_name, name) {
            skipped.push((name.to_owned(), reason.to_string()));
        }
    }
    if listed == 0 {
        return Check::fail(
            "0 tools discovered",
            "server returned an empty tool list",
            "check server config",
        );
    }
    if skipped.is_empty() {
        return Check::pass(format!("{listed} tools discovered"), "");
    }
    let detail = skipped
        .iter()
        .map(|(name, reason)| format!("{name} ({reason})"))
        .collect::<Vec<_>>()
        .join("; ");
    Check::fail(
        format!(
            "{listed} tools listed, {} skipped by session admission",
            skipped.len()
        ),
        detail,
        "session catalog omits these tools; rename the server or tool so each segment is a valid MCP id",
    )
}

fn format_mcp_error(label: &str, err: &mcp_servers::McpError) -> Check {
    use mcp_servers::McpError;
    match err {
        McpError::Timeout { timeout_secs, .. } => Check::fail(
            "server timed out",
            format!("no response within {}s", timeout_secs),
            "try increasing startup_timeout_sec in config.toml",
        ),
        McpError::SpawnFailed { source, .. } => Check::fail(
            "spawn failed",
            source.to_string(),
            "check command and permissions",
        ),
        McpError::HandshakeFailed { source, .. } => {
            Check::fail("handshake failed", source.to_string(), "check server logs")
        }
        _ => Check::fail(label, err.to_string(), "check server logs"),
    }
}

// ── Per-server orchestration ────────────────────────────────────

/// Skip-verdict checks for a server the doctor will not start, policy first;
/// only the first barrier keeps its hint (later hints are dead ends).
fn skip_verdict_checks(
    block_detail: Option<(&str, String)>,
    untrusted: bool,
    disabled: bool,
) -> Vec<Check> {
    let mut checks = Vec::new();
    // The renderer parenthesises the detail, so the source goes there and the rule in the label.
    if let Some((rule, source)) = block_detail {
        checks.push(Check::fail_no_hint(
            format!("blocked by organization policy — {rule}"),
            source,
        ));
    }
    if untrusted {
        let label = "folder untrusted";
        let detail = "repo-local (project-scoped) server not started for an untrusted folder";
        checks.push(if checks.is_empty() {
            Check::fail(
                label,
                detail,
                "re-run with --trust to allow repo-local servers",
            )
        } else {
            Check::fail_no_hint(label, detail)
        });
    }
    if disabled {
        let label = "disabled in config";
        let detail = "server is disabled in config.toml";
        checks.push(if checks.is_empty() {
            Check::fail(
                label,
                detail,
                "set enabled = true or remove from disabled_mcp_servers",
            )
        } else {
            Check::fail_no_hint(label, detail)
        });
    }
    checks
}

fn describe_server(server: &agent_client_protocol::McpServer) -> (String, String) {
    (
        mcp_servers::mcp_transport_str(server).to_string(),
        mcp_servers::mcp_target_str(server),
    )
}

async fn check_server(
    server: agent_client_protocol::McpServer,
    source_label: &str,
    cwd: &Path,
) -> McpServerStatus {
    let name = mcp_servers::mcp_server_name(&server).to_string();
    let (transport, target) = describe_server(&server);

    let mut checks = Vec::new();

    if let agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio {
        ref command,
        ..
    }) = server
    {
        let check = check_command_exists(&command.to_string_lossy());
        let ok = check.passed;
        checks.push(check);
        if !ok {
            return McpServerStatus {
                name,
                transport,
                target,
                source: source_label.to_string(),
                checks,
                healthy: false,
            };
        }
    }

    match check_server_start(server, cwd).await {
        Err(check) => {
            checks.push(check);
        }
        Ok((client, check)) => {
            checks.push(check);
            match check_handshake(&client).await {
                Err(check) => {
                    checks.push(check);
                }
                Ok((service, check)) => {
                    checks.push(check);
                    checks.push(check_tools_list(&name, &service).await);
                }
            }
            // Client drops here, killing the child process via kill_on_drop
        }
    }

    let healthy = checks.iter().all(|c| c.passed);
    McpServerStatus {
        name,
        transport,
        target,
        source: source_label.to_string(),
        checks,
        healthy,
    }
}

// ── Policy verdicts ─────────────────────────────────────────────

/// Managed-policy verdicts for every discovered definition the session merge would drop, keyed
/// by server name; personal disable is ignored — the caller co-reports it.
fn policy_blocked_reasons(
    discovered: &[DiscoveredServer],
    project_names: &std::collections::HashSet<String>,
    ms: &distill_workspace::permission::resolution::ManagedSettings,
) -> HashMap<String, distill_workspace::permission::resolution::McpBlockReason> {
    crate::session::managed_mcp::mcp_blocked_reasons(
        discovered.iter().map(|d| {
            let subject =
                crate::session::managed_mcp::mcp_subject(&d.server, &d.source, project_names);
            (mcp_servers::mcp_server_name(&d.server), &d.server, subject)
        }),
        ms,
    )
}

/// Definitions `grok mcp list`/`enable` judge, with subjects: the TOML walk blind to `enabled` and
/// folder trust (so disabled or untrusted-repo definitions keep a verdict), then non-TOML tiers.
fn policy_subjects(
    cwd: &Path,
) -> Vec<(
    String,
    agent_client_protocol::McpServer,
    distill_workspace::permission::resolution::McpSubject,
)> {
    use crate::util::config::{
        MCP_SCOPE_PROJECT, McpEnabledFilter, load_mcp_preferences,
        load_mcp_server_configs_with_project, materialize_mcp_config,
    };
    use distill_tools::types::config_source::ConfigSource;
    let project_names = crate::agent::folder_trust::project_scoped_mcp_names(cwd);
    let preferences = load_mcp_preferences().file();
    let sub = &crate::config::expand_env_vars_in_string;
    let mut subjects: Vec<_> = load_mcp_server_configs_with_project(cwd)
        .into_iter()
        .filter_map(|(name, (config, scope))| {
            let server = materialize_mcp_config(
                &name,
                config.clone(),
                &preferences,
                sub,
                McpEnabledFilter::Ignore,
            )
            // Setup-required or invalid servers still get a verdict on the name and transport
            // as configured; otherwise the enable gate and `mcp list` would treat them as allowed.
            .or_else(|| {
                let mut raw = config;
                raw.enabled = true;
                raw.setup = None;
                raw.expand_strings(sub);
                raw.to_acp_mcp_server(&name)
            })?;
            let subject = crate::session::managed_mcp::mcp_subject_for_tier(
                &name,
                scope != MCP_SCOPE_PROJECT,
                &project_names,
            );
            Some((name, server, subject))
        })
        .collect();
    let plugin_registry = crate::util::config::load_cli_plugin_registry(cwd);
    for (server, source) in crate::session::managed_mcp::merge_managed_mcp_servers_sourced(
        cwd,
        Some(&plugin_registry),
        &distill_tools::types::compat::CompatConfig::default(),
    ) {
        // The merge re-adds enabled TOML servers; the walk above already judged every TOML one.
        if matches!(source, ConfigSource::ConfigToml { .. }) {
            continue;
        }
        let subject = crate::session::managed_mcp::mcp_subject(&server, &source, &project_names);
        subjects.push((
            mcp_servers::mcp_server_name(&server).to_string(),
            server,
            subject,
        ));
    }
    subjects
}

/// The verdict map for `grok mcp list`, keyed by server name.
pub fn policy_blocked_servers(
    cwd: &Path,
) -> HashMap<String, distill_workspace::permission::resolution::McpBlockReason> {
    crate::session::managed_mcp::mcp_blocked_reasons(
        policy_subjects(cwd)
            .iter()
            .map(|(name, server, subject)| (name.as_str(), server, *subject)),
        distill_workspace::permission::resolution::managed_settings(),
    )
}

/// The `grok mcp enable` gate: the org-policy refusal the TUI and `grok mcp add` emit, or `None`.
pub fn policy_enable_refusal(cwd: &Path, name: &str) -> Option<String> {
    let ms = distill_workspace::permission::resolution::managed_settings();
    policy_subjects(cwd)
        .iter()
        .filter(|(candidate, _, _)| candidate == name)
        .find_map(|(_, server, subject)| {
            crate::extensions::mcp::policy_enable_error(ms, server, *subject)
        })
}

/// Which config file a new MCP definition is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpWriteScope {
    /// `~/.grok/config.toml`.
    User,
    /// `./.grok/config.toml`.
    Project,
}

/// Add-time policy gate for a NEW server definition (`grok mcp add`; the TUI upsert applies the
/// same rule): grok-native unless a project source is the write target or claims the name.
pub fn policy_add_refusal(
    cwd: &Path,
    name: &str,
    config: &crate::util::config::McpServerConfig,
    scope: McpWriteScope,
) -> Option<String> {
    policy_add_refusal_with(
        distill_workspace::permission::resolution::managed_settings(),
        crate::agent::folder_trust::project_scoped_mcp_names(cwd),
        scope,
        name,
        config,
    )
}

/// [`policy_add_refusal`] over injected inputs (the OnceLock seam).
fn policy_add_refusal_with(
    ms: &distill_workspace::permission::resolution::ManagedSettings,
    mut project_names: std::collections::HashSet<String>,
    scope: McpWriteScope,
    name: &str,
    config: &crate::util::config::McpServerConfig,
) -> Option<String> {
    let server = config.to_acp_mcp_server(name)?;
    if scope == McpWriteScope::Project {
        // The definition lands in a project source, so it is project-claimed before it exists there.
        project_names.insert(name.to_owned());
    }
    let subject = crate::session::managed_mcp::mcp_subject_for_tier(name, true, &project_names);
    crate::extensions::mcp::policy_enable_error(ms, &server, subject)
}

// ── Entry point ─────────────────────────────────────────────────

pub async fn run_doctor(cwd: &Path, name_filter: Option<&str>) -> DoctorReport {
    let (mut sources, discovered) = discover_servers(cwd);

    let ms = distill_workspace::permission::resolution::managed_settings();
    let allowlist = &ms.mcp_allowlist;
    if allowlist.is_restricted() {
        let paths = allowlist.source_paths();
        let path = if paths.is_empty() {
            "managed-settings.json".to_string()
        } else {
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        // A deny-only policy isn't an allowlist; a zero-entry managed-only
        // lockdown reads as "server policy".
        let allow_entries: usize = allowlist.sources.iter().map(|s| s.entries().count()).sum();
        let deny_entries: usize = allowlist
            .sources
            .iter()
            .map(|s| s.deny_entries().count())
            .sum();
        let label = match (allow_entries, deny_entries) {
            (1.., 0) => "server allowlist",
            (0, 1..) => "server denylist",
            _ => "server policy",
        };
        sources.push(ConfigSourceStatus {
            path: format!("{label} ({})", path),
            status: ConfigSourceState::Found {
                // Same sums as the label so the row can't disagree with itself.
                server_count: allow_entries + deny_entries,
            },
        });
    }

    let all_server_names: Vec<String> = discovered
        .iter()
        .map(|d| mcp_servers::mcp_server_name(&d.server).to_string())
        .collect();

    let to_probe: Vec<DiscoveredServer> = if let Some(filter) = name_filter {
        discovered
            .into_iter()
            .filter(|d| mcp_servers::mcp_server_name(&d.server) == filter)
            .collect()
    } else {
        discovered
    };

    let disabled_names = crate::util::config::disabled_mcp_server_names(cwd);

    // Folder-trust gate: `grok mcp doctor` actually STARTS each server (`check_server_start`) In an untrusted clone that would spawn the repo's project-scoped servers
    // Resolve the doctor cwd once (no prompt), then skip (do not start) any project-scoped server when untrusted
    // Uses the same name lookup (`project_scoped_mcp_names`) as the session/agent-pool gates `remote = None` is intentional: standalone `grok mcp doctor` has no loaded `RemoteSettings` A remote-only org opt-out (`folder_trust_enabled = false`) isn't seen here Gating conservatively (treating the feature as enabled) is the deliberate fail-secure choice
    crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    // One project-config walk serves both the folder-trust skip set and the
    // policy subject classification below.
    let project_names = crate::agent::folder_trust::project_scoped_mcp_names(cwd);
    let untrusted_project: std::collections::HashSet<String> =
        if crate::agent::folder_trust::project_scope_allowed(cwd) {
            std::collections::HashSet::new()
        } else {
            project_names.clone()
        };

    const PROBE_CONCURRENCY: usize = 8;

    // Classify each server's origin the way the session merge does, so the
    // doctor's verdicts match what actually loads.
    let blocked = policy_blocked_reasons(&to_probe, &project_names, ms);

    use futures::StreamExt;
    let results: Vec<McpServerStatus> = futures::stream::iter(to_probe)
        .map(|d| {
            let label = d.source.display_label();
            let name = mcp_servers::mcp_server_name(&d.server).to_string();
            let block_detail = blocked
                .get(&name)
                .map(|reason| (reason.rule(), reason.source().display().to_string()));
            let disabled = disabled_names.contains(&name);
            let untrusted = untrusted_project.contains(&name);
            async move {
                let skip_checks = skip_verdict_checks(block_detail, untrusted, disabled);
                if !skip_checks.is_empty() {
                    let (transport, target) = describe_server(&d.server);
                    return McpServerStatus {
                        name,
                        transport,
                        target,
                        source: label,
                        checks: skip_checks,
                        healthy: false,
                    };
                }
                check_server(d.server, &label, cwd).await
            }
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;
    let healthy_count = results.iter().filter(|s| s.healthy).count();
    let failing_count = results.len() - healthy_count;

    DoctorReport {
        sources,
        servers: results,
        all_server_names,
        healthy_count,
        failing_count,
    }
}

// ── Human-readable output ───────────────────────────────────────

pub fn print_report(report: &DoctorReport) {
    println!();
    println!("MCP Doctor");
    println!();

    println!("  Config sources");
    for source in &report.sources {
        let status = match &source.status {
            ConfigSourceState::Found { server_count } => {
                format!(
                    "{} server{}",
                    server_count,
                    if *server_count == 1 { "" } else { "s" }
                )
            }
            ConfigSourceState::NotFound => "not found".to_string(),
            ConfigSourceState::Skipped { reason } => format!("skipped ({})", reason),
        };
        println!("    {:<40} {}", source.path, status);
    }
    println!();

    if report.servers.is_empty() {
        println!("  No MCP servers configured.");
        println!("  Run `grok mcp add --help` to get started.");
        println!();
        return;
    }

    for server in &report.servers {
        println!(
            "  {} ({}: {})",
            server.name, server.transport, server.target
        );
        for check in &server.checks {
            let icon = if check.passed { "\u{2713}" } else { "\u{2717}" };
            let detail = check.detail.as_deref().unwrap_or("");
            if detail.is_empty() {
                println!("    {} {}", icon, check.label);
            } else {
                println!("    {} {} ({})", icon, check.label, detail);
            }
            if let Some(hint) = &check.hint {
                println!("    \u{2192} {}", hint);
            }
        }
        println!();
    }

    println!(
        "Found {} healthy, {} failing.{}",
        report.healthy_count,
        report.failing_count,
        if report.failing_count > 0 {
            " Run `grok mcp doctor --json` for full diagnostics."
        } else {
            ""
        }
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_servers::McpError;

    /// A hint must only survive on the first barrier.
    #[test]
    fn skip_hints_survive_only_on_the_first_barrier() {
        let checks = skip_verdict_checks(
            Some((
                "matches deniedMcpServers",
                "/etc/grok/managed_config.toml".into(),
            )),
            true,
            true,
        );
        let [first, ..] = checks.as_slice() else {
            panic!("expected skip checks: {checks:?}");
        };
        assert_eq!(checks.len(), 3);
        assert_eq!(
            first.label,
            "blocked by organization policy — matches deniedMcpServers"
        );
        assert_eq!(
            first.detail.as_deref(),
            Some("/etc/grok/managed_config.toml")
        );
        assert!(
            checks.iter().all(|c| c.hint.is_none()),
            "org policy is terminal; no local hint can help: {checks:?}"
        );

        let checks = skip_verdict_checks(None, true, true);
        let [first, second, ..] = checks.as_slice() else {
            panic!("expected skip checks: {checks:?}");
        };
        assert!(first.hint.is_some(), "--trust is the actionable step");
        assert!(second.hint.is_none(), "re-enable hint is a dead end");

        let checks = skip_verdict_checks(None, false, true);
        assert!(
            checks.first().is_some_and(|c| c.hint.is_some()),
            "plain disable keeps its hint"
        );
    }

    /// Project-declared servers must count under their own config file.
    #[test]
    fn toml_declaring_paths_attributes_project_servers() {
        let user: toml::Value = toml::from_str(
            "[mcp_servers.home1]\ncommand = 'a'\n[mcp_servers.home2]\ncommand = 'b'\n",
        )
        .unwrap();
        let proj: toml::Value = toml::from_str(
            "[mcp_servers.projsrv]\ncommand = 'c'\n[mcp_servers.home2]\ncommand = 'override'\n",
        )
        .unwrap();
        let user_path = Path::new("/home/u/.grok/config.toml");
        let proj_path = std::path::PathBuf::from("/repo/.grok/config.toml");

        let declaring = toml_declaring_paths(user_path, Some(&user), &[(proj_path.clone(), proj)]);
        assert_eq!(declaring.get("home1").map(|p| p.as_path()), Some(user_path));
        assert_eq!(declaring.get("projsrv"), Some(&proj_path));
        // The nearest definition wins a shared name, matching the merge.
        assert_eq!(declaring.get("home2"), Some(&proj_path));
    }

    /// Pins the CLI add gate seam: a deny-matching new definition gets the org-policy refusal;
    /// an unrelated URL passes.
    #[test]
    fn add_refusal_blocks_deny_matching_new_server() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, ManagedSettings, McpServerAllowlist, McpServerPolicy,
        };
        let mut ms = ManagedSettings::default();
        ms.mcp_allowlist = McpServerPolicy::single(McpServerAllowlist::new(
            vec![],
            vec![AllowedMcpServer::Http {
                url_pattern: "http://127.0.0.1:59987/*".into(),
            }],
            Some(std::path::PathBuf::from("/test/managed_config.toml")),
        ));
        let none = std::collections::HashSet::new();

        let denied: crate::util::config::McpServerConfig =
            serde_json::from_value(serde_json::json!({ "url": "http://127.0.0.1:59987/evil" }))
                .unwrap();
        let refusal =
            policy_add_refusal_with(&ms, none.clone(), McpWriteScope::User, "evil", &denied)
                .expect("deny-matching add must be refused");
        assert!(refusal.contains("organization policy"), "got: {refusal}");

        let allowed: crate::util::config::McpServerConfig =
            serde_json::from_value(serde_json::json!({ "url": "https://ok.example/mcp" })).unwrap();
        assert_eq!(
            policy_add_refusal_with(&ms, none, McpWriteScope::User, "ok", &allowed),
            None
        );
    }

    /// Resets the process-global Claude import marker cache on drop (mirrors the module-private
    /// claude_import::tests::MarkerGuard).
    struct MarkerCacheReset;
    impl Drop for MarkerCacheReset {
        fn drop(&mut self) {
            crate::claude_import::reset_marker_cache_for_test();
        }
    }

    /// The TOML walk and the merge walk partition the subjects: a config.toml server is judged
    /// once, and the merge walk still contributes the non-TOML tiers.
    #[test]
    #[serial_test::serial]
    fn policy_subjects_judge_each_definition_once() {
        // A real `[claude_compat] imported` marker would cut off `.mcp.json`, so pin the cache to
        // "not imported"; the TOML seed is project-scoped since distill_home() is a OnceLock.
        let _reset = MarkerCacheReset;
        crate::claude_import::refresh_marker_cache(false);
        let repo = tempfile::tempdir().unwrap();
        git2::Repository::init(repo.path()).unwrap();
        std::fs::create_dir_all(repo.path().join(".grok")).unwrap();
        std::fs::write(
            repo.path().join(".grok/config.toml"),
            "[mcp_servers.corp]\nurl = \"https://corp.example/mcp\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join(".mcp.json"),
            r#"{"mcpServers": {"repo-tool": {"type": "http", "url": "https://repo.example/mcp"}}}"#,
        )
        .unwrap();

        let names: Vec<String> = policy_subjects(repo.path())
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(
            names.iter().filter(|n| *n == "corp").count(),
            1,
            "TOML server judged once: {names:?}"
        );
        assert!(names.iter().any(|n| n == "repo-tool"), "got: {names:?}");
    }

    /// `grok mcp add --scope project` writes a project source, so a fresh name is judged
    /// project-scoped: the project-MCP pin refuses it while the same user-scope add passes.
    #[test]
    fn add_refusal_applies_project_pin_to_project_scope_writes() {
        use distill_workspace::permission::resolution::{
            ManagedSettings, PolicyLayerOwnership, PolicyPin,
        };
        let mut ms = ManagedSettings::default();
        ms.project_mcp = PolicyPin::Disabled {
            source: std::path::PathBuf::from("/etc/grok/managed_config.toml"),
            ownership: PolicyLayerOwnership::Admin,
        };
        let none = std::collections::HashSet::new();
        let config: crate::util::config::McpServerConfig =
            serde_json::from_value(serde_json::json!({ "url": "https://fresh.example/mcp" }))
                .unwrap();

        let refusal =
            policy_add_refusal_with(&ms, none.clone(), McpWriteScope::Project, "fresh", &config)
                .expect("project-scope add of a fresh name must hit the project pin");
        assert!(
            refusal.contains("organization policy") && refusal.contains("managed_config.toml"),
            "got: {refusal}"
        );
        assert_eq!(
            policy_add_refusal_with(&ms, none, McpWriteScope::User, "fresh", &config),
            None,
            "the same definition in user config.toml is grok-native and unpinned"
        );
    }

    #[test]
    fn tools_list_admission_reports_session_skips() {
        // Digit-leading tool segments are admitted; letter/_ first-char is the server prefix.
        let check = tools_list_admission_check("linear", ["list_issues", "2fa_enable"]);
        assert!(check.passed, "{check:?}");
        assert_eq!(check.label, "2 tools discovered");

        let check = tools_list_admission_check("123bad", ["lookup"]);
        assert!(!check.passed);
        assert_eq!(
            check.label,
            "1 tools listed, 1 skipped by session admission"
        );
        assert!(
            check
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("lookup") && d.contains("server name")),
            "{check:?}"
        );

        let check = tools_list_admission_check("linear", ["list_issues", "bad.tool"]);
        assert!(!check.passed);
        assert_eq!(
            check.label,
            "2 tools listed, 1 skipped by session admission"
        );
        assert!(
            check
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("bad.tool")),
            "{check:?}"
        );

        // 31-char server + 37-char tool is 70 qualified chars: provider-64 rejects,
        // session admission keeps it.
        let check = tools_list_admission_check(
            "sL_xxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            ["t0_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"],
        );
        assert!(check.passed, "{check:?}");
        assert_eq!(check.label, "1 tools discovered");
    }

    #[test]
    fn timeout_gets_specific_hint() {
        let err = McpError::Timeout {
            server: "test".into(),
            timeout_secs: 5,
        };
        let check = format_mcp_error("ignored", &err);
        assert_eq!(check.label, "server timed out");
        assert!(
            check
                .hint
                .as_deref()
                .unwrap()
                .contains("startup_timeout_sec")
        );
    }

    #[test]
    fn non_timeout_uses_caller_label() {
        let check = format_mcp_error("handshake failed", &McpError::ClientError("boom".into()));
        assert_eq!(check.label, "handshake failed");
        assert_eq!(check.detail.as_deref(), Some("MCP client error: boom"));
    }

    #[test]
    fn spawn_failed_shows_io_error() {
        let err = McpError::SpawnFailed {
            server: "test".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory"),
        };
        let check = format_mcp_error("ignored", &err);
        assert_eq!(check.label, "spawn failed");
        assert!(check.detail.as_deref().unwrap().contains("No such file"));
    }
}
