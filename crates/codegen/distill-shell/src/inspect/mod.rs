// Modified for Distill by Samuel Fajreldines, 2026.
//! `grok inspect`: configuration introspection.
//!
//! Shows everything Grok discovers in the current directory.
//! That covers project instructions, permissions, hooks, skills, agents, plugins, MCP servers, LSP config, and config.toml sources.
//! Supports `--json` for machine output.

mod compat;

pub use compat::{CompatEntryStatus, CompatSource, ExternalCompatEntry, ExternalCompatReport};
use compat::{
    derive_vendor, instruction_compat_status, resolve_inspect_compat, vendor_compat_status,
    vendor_tag,
};

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use distill_login::ForceLoginTeam;
use distill_tools::types::config_source::ConfigSource;
use distill_tools::util::truncate::estimate_tokens;
use distill_workspace::permission::resolution::{
    ManagedSettings, YoloPolicyLock, claude_bypass_lock_request,
};

const TREE: &str = "\u{2514}";

/// Coarse scope label for project instructions and plugin entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Project,
    User,
    Global,
    Plugin,
    Builtin,
    Cli,
    Config,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Project => "project",
            Self::User => "user",
            Self::Global => "global",
            Self::Plugin => "plugin",
            Self::Builtin => "builtin",
            Self::Cli => "cli",
            Self::Config => "config",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InspectReport {
    pub grok_version: String,
    pub channel: String,
    pub cwd: String,
    pub project_root: Option<String>,
    /// Folder-trust verdict for `cwd`: when false, repo-local project hooks, plugins, MCP/LSP, instructions, and skills are gated out of the listings below.
    pub project_trusted: bool,
    pub project_instructions: Vec<InstructionFile>,
    pub permissions: PermissionsReport,
    pub login_policy: LoginPolicyReport,
    pub hooks: Vec<HookEntry>,
    pub skills: Vec<SkillEntry>,
    pub agents: Vec<AgentEntry>,
    pub plugins: Vec<PluginEntry>,
    pub marketplaces: Vec<MarketplaceEntry>,
    pub mcp_servers: Vec<McpServerEntry>,
    pub lsp_servers: Vec<LspServerEntry>,
    pub config_sources: ConfigSources,
    pub external_compat: ExternalCompatReport,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config_warnings: Vec<crate::agent::config_model_override_parse::ConfigWarning>,
    /// Invalid or ignored `[mcp_servers.*]` entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_config_problems: Vec<crate::util::config::McpServerConfigProblem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstructionFile {
    pub path: String,
    pub scope: Scope,
    pub file_type: String,
    pub size_bytes: usize,
    /// Estimated token count (chars / 4).
    pub approx_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// True when this entry's vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
}

/// How the `allowManagedMcpServersOnly` lockdown binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedOnlyScope {
    /// No source pins the lockdown.
    Off,
    /// Only the advisory Claude file pins it: binds foreign-defined servers,
    /// grok-native servers exempt.
    Advisory,
    /// A grok TOML layer pins it: binds every server.
    Enforced,
}

impl ManagedOnlyScope {
    fn resolve(policy: &distill_workspace::permission::resolution::McpServerPolicy) -> Self {
        use distill_workspace::permission::resolution::PolicySubjectOrigin;
        // Order is load-bearing: managed_only(Foreign) is true for native sources too.
        if policy.managed_only(PolicySubjectOrigin::GrokNative) {
            Self::Enforced
        } else if policy.managed_only(PolicySubjectOrigin::Foreign) {
            Self::Advisory
        } else {
            Self::Off
        }
    }
}

/// One full-lockdown policy source and how it binds.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LockdownSource {
    pub source: String,
    /// The advisory vendor file: binds foreign-defined subjects only, grok-native exempt.
    pub advisory: bool,
}

impl LockdownSource {
    /// Human row; `exempt` names what an advisory source does not bind.
    fn human_row(&self, exempt: &str) -> String {
        if self.advisory {
            format!("{} (advisory; grok-native {exempt} exempt)", self.source)
        } else {
            self.source.clone()
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionsReport {
    pub sources: Vec<String>,
    pub loaded: usize,
    pub skipped: Vec<SkippedRule>,
    pub mcp_server_allowlist: Vec<String>,
    /// Sources whose MCP policy is a full lockdown: every server they bind is
    /// blocked, even if a valid sibling key still contributes allow entries.
    pub mcp_lockdown_sources: Vec<LockdownSource>,
    /// `allowManagedMcpServersOnly` lockdown and how it binds.
    pub mcp_managed_servers_only: ManagedOnlyScope,
    pub marketplace_allowlist: Vec<String>,
    /// Sources whose strict marketplace list has zero usable URLs (explicit
    /// `[]`, malformed, or every entry unsupported): a marketplace lockdown.
    pub marketplace_lockdown_sources: Vec<LockdownSource>,
    /// Marketplaces pinned via managed `extraKnownMarketplaces` (`name (url[@ref])`).
    /// Always emitted, like the allowlists above, so the JSON schema is stable.
    pub managed_marketplaces: Vec<String>,
    /// Platform path for managed-settings.json vendor policy (None on unsupported OS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_settings_path: Option<String>,
    /// Whether that file exists on disk.
    /// Always emitted, so a JSON consumer can distinguish "absent" from "present" without string-matching.
    pub managed_settings_exists: bool,
    /// Whether the runtime actually loaded that file into policy (`exists` can be true while this is false for an unreadable/malformed file).
    /// Always emitted.
    pub managed_settings_active: bool,
    /// One row per active policy clamp; other requirements-pinned fields are not listed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enforced: Vec<EnforcedPolicy>,
    /// A Claude managed-settings `disableBypassPermissionsMode` request, which grok deliberately does not enforce ([`claude_bypass_lock_request`](distill_workspace::permission::resolution::claude_bypass_lock_request)).
    /// Always emitted so "no request" is distinguishable from an old binary.
    pub claude_bypass_lock_advisory: bool,
}

/// One policy-enforced setting.
/// Structured for `--json`; the human view derives its line from these fields (see `enforced_label`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnforcedPolicy {
    pub setting: EnforcedSetting,
    /// The enforced value.
    pub enabled: bool,
    /// Provenance label: the full path of the policy file that set it
    /// (requirements layer for alwaysApprove, managed-settings.json for
    /// telemetry/feedback), or the diskless macOS MDM source id.
    pub source: String,
}

/// Policy-clamped knob; the camelCase serialization is the stable `--json` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum EnforcedSetting {
    AlwaysApprove,
    Telemetry,
    Feedback,
    /// `enableAllProjectMcpServers = false` pin: project MCP sources ignored.
    ProjectMcpServers,
    /// `plugin_auto_update = false` pin: session-start plugin auto-update off.
    PluginAutoUpdate,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkippedRule {
    pub rule: String,
    pub reason: String,
}

/// Enterprise login-hardening policy resolved from `[grok_com_config]` (TOML and env).
/// Shown so admins can verify the deployment loaded it.
/// The team pin is admin policy, not a secret, so it is shown verbatim.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoginPolicyReport {
    /// Raw `disable_api_key_auth` knob (env `GROK_DISABLE_API_KEY_AUTH`).
    pub disable_api_key_auth: Option<bool>,
    /// Configured team pin: single string, list, or null when unset.
    pub force_login_team_uuid: Option<ForceLoginTeam>,
    /// Resolved verdict: true when either knob forces first-party login.
    pub api_key_auth_disabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HookEntry {
    pub event: String,
    pub hook_type: String,
    pub target: String,
    pub source: ConfigSource,
    pub matcher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// True when this entry's vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkillEntry {
    pub name: String,
    pub description: String,
    pub source: ConfigSource,
    pub user_invocable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// True when disabled by `[skills].disabled` config or when this entry's vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
    /// Bare name this skill lost (`login`, `commit`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collides_with: Option<String>,
    /// Qualified invocation when [`Self::collides_with`] is set.
    /// Absent when that name is contested too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocable_as: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentEntry {
    pub name: String,
    pub description: String,
    pub source: ConfigSource,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntry {
    pub name: String,
    pub scope: Scope,
    pub path: String,
    pub enabled: bool,
    pub provides: PluginProvides,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginProvides {
    pub skills: usize,
    pub agents: usize,
    pub hooks: bool,
    pub mcp_servers: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarketplaceEntry {
    pub name: String,
    pub path: String,
    pub enabled_plugins: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerEntry {
    pub name: String,
    pub transport: String,
    pub target: String,
    pub source: ConfigSource,
    /// True when this entry's vendor surface is disabled by compat config.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_status: Option<CompatEntryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LspServerEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub source: ConfigSource,
    pub extensions: Vec<String>,
    /// True when this project-scoped server would be skipped (untrusted folder).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub untrusted: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigSources {
    /// Built by the same resolvers the runtime uses (`ConfigLayers`, `requirements_layers`), so system and MDM layers and precedence are included.
    /// A layer counts as empty only after the loader strips keys like version_overrides and fail_closed.
    pub layers: Vec<ConfigLayer>,
}

/// A single config layer entry for `grok inspect`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigLayer {
    /// Logical role of the layer: "system-managed", "managed", "user", "system-requirements", "requirements", "mdm", or "project".
    pub role: String,
    pub path: String,
    /// "empty" or "parse error" when the on-disk file does not contribute effective config (after the real loader's processing).
    /// Omitted when the layer is present and contributes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub async fn inspect(cwd: &Path, json: bool) -> anyhow::Result<()> {
    let report = build_report(cwd).await;
    write_inspect(&report, json, &mut std::io::stdout().lock())
}

/// A closed stdout (`grok inspect | head`) is a clean stop.
fn write_inspect(report: &InspectReport, json: bool, out: &mut impl Write) -> anyhow::Result<()> {
    let written = if json {
        writeln!(out, "{}", serde_json::to_string_pretty(report)?)
    } else {
        print_human(report, out)
    };
    match written {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

async fn build_report(cwd: &Path) -> InspectReport {
    let effective_config_result = crate::config::load_effective_config();
    let effective_config = effective_config_result
        .as_ref()
        .cloned()
        .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()));
    // Parse compatibility separately so malformed cells cannot block unrelated sections.
    let mut config_without_compat = effective_config.clone();
    if let Some(table) = config_without_compat.as_table_mut() {
        table.remove("compat");
    }
    let parsed_config = crate::agent::config::Config::new_from_toml_cfg(&config_without_compat);
    // A config that does not parse is the answer `inspect` exists to give, so keep the reason rather than reporting an empty config as a clean one
    let config_parse_error = parsed_config.as_ref().err().cloned();
    let parsed_config = parsed_config.ok();

    let git_root = git2::Repository::discover(cwd)
        .ok()
        .and_then(|r| r.workdir().map(|p| p.to_path_buf()));

    // Route through the live folder-trust gate rather than a raw store read; no session resolve has run for a one-shot `inspect`
    // `remote = None`: env/user/managed opt-out is honored, but a remote kill-switch is not consulted on this report-only path
    crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let project_trusted = crate::agent::folder_trust::project_scope_allowed(cwd);

    let trust_store = distill_agent::plugins::TrustStore::load();
    let mut plugins_cfg: crate::agent::config::PluginsConfig = effective_config
        .get("plugins")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or_default();
    plugins_cfg.merge_claude_enabled_plugins(Some(cwd));
    let mut plugin_config = plugins_cfg.to_discovery_config();
    // Project plugins gate on the same folder-trust verdict as hooks and the live session/doctor sites
    // The listing's `enabled` flags therefore match runtime gating
    let discovered_plugins = distill_agent::plugins::discover_plugins(
        Some(cwd),
        &plugin_config,
        &trust_store,
        project_trusted,
    );
    plugin_config.populate_plugin_lists(&discovered_plugins);

    let plugin_registry = distill_agent::plugins::PluginRegistry::from_discovered(
        discovered_plugins.clone(),
        &plugin_config.disabled,
        &plugin_config.enabled,
    );

    let external_compat = resolve_inspect_compat(effective_config_result.as_ref().map_err(|_| ()));

    // This is the same `[skills]` table the runtime loads: `paths` skills appear, `ignore`d ones are hidden, `disabled` ones show as disabled
    let skills_config = crate::config::parse_skills_config(&effective_config);
    // Same for `[paths]`: rules from `extra_rule_dirs` appear, as they do in a session
    let paths_config: crate::agent::config::PathsConfig = effective_config
        .get("paths")
        .and_then(|v| v.clone().try_into().ok())
        .unwrap_or_default();

    let (mut instructions, permissions, mut skills) = tokio::join!(
        list_instructions(cwd, &paths_config, project_trusted),
        list_permissions(cwd, project_trusted),
        list_skills(cwd, &plugin_registry, &skills_config, project_trusted),
    );

    // Attach local compatibility status to each discovered vendor entry.
    for entry in &mut instructions {
        entry.compatibility_status =
            instruction_compat_status(&entry.vendor, &entry.file_type, &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    for entry in &mut skills {
        entry.compatibility_status =
            vendor_compat_status(&entry.vendor, "skills", &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    let mut hooks = list_hooks(git_root.as_deref(), project_trusted, &discovered_plugins);
    for entry in &mut hooks {
        entry.compatibility_status = vendor_compat_status(&entry.vendor, "hooks", &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    let agents = list_agents(cwd, &plugin_registry);
    let plugins = list_plugins(&discovered_plugins);
    let marketplaces = list_marketplaces(git_root.as_deref());
    let mut mcp = list_mcp_servers(cwd, &plugin_registry);
    for entry in &mut mcp {
        entry.compatibility_status = vendor_compat_status(&entry.vendor, "mcps", &external_compat);
        entry.disabled |= entry.compatibility_status == Some(CompatEntryStatus::Disabled);
    }
    let lsp = list_lsp_servers(cwd, &discovered_plugins);
    let configs = list_config_sources(cwd);
    let mut config_warnings = parsed_config
        .as_ref()
        .map(|c| c.config_warnings.clone())
        .unwrap_or_default();
    if let Some(error) = config_parse_error {
        config_warnings.push(
            crate::agent::config_model_override_parse::ConfigWarning::config_key(
                "config".to_owned(),
                crate::agent::config_model_override_parse::ConfigWarningKind::InvalidValue,
                format!("the config does not load: {error}"),
            ),
        );
    }
    let mcp_config_problems = crate::util::config::load_mcp_server_problems_with_project(cwd);

    InspectReport {
        grok_version: distill_version::VERSION.to_string(),
        channel: crate::util::config::channel_name_from_cache()
            .unwrap_or("unknown")
            .to_string(),
        cwd: cwd.display().to_string(),
        project_root: git_root.map(|p| p.display().to_string()),
        project_trusted,
        project_instructions: instructions,
        permissions,
        login_policy: login_policy_report(parsed_config.as_ref()),
        hooks,
        skills,
        agents,
        plugins,
        marketplaces,
        mcp_servers: mcp,
        lsp_servers: lsp,
        config_sources: configs,
        external_compat,
        config_warnings,
        mcp_config_problems,
    }
}

fn has_rules_directory(file_path: &str, config_dir: &str) -> bool {
    let mut previous = None;
    for component in file_path
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
    {
        if previous == Some(config_dir) && component == "rules" {
            return true;
        }
        previous = Some(component);
    }
    false
}

fn instruction_scope(
    file_path: &str,
    distill_home: &Path,
    vendor_homes: &[(PathBuf, bool)],
    workspace_root: &Path,
) -> Scope {
    if crate::util::is_user_instruction_path(
        Path::new(file_path),
        distill_home,
        vendor_homes,
        &[workspace_root],
    ) {
        Scope::Global
    } else {
        Scope::Project
    }
}

fn instruction_file_type(
    file_path: &str,
    distill_home: &Path,
    claude_imported: bool,
) -> &'static str {
    let path = Path::new(file_path);
    if path
        .parent()
        .is_some_and(|parent| parent == distill_home.join("rules"))
        || has_rules_directory(file_path, ".grok")
        || has_rules_directory(file_path, ".cursor")
        || (!claude_imported && has_rules_directory(file_path, ".claude"))
    {
        "rules"
    } else {
        "agents_md"
    }
}

/// Wraps the production instruction discovery (`agents_md::read_agents_config_with_paths`).
/// `paths` is the same `[paths]` table a session loads, so inspect lists exactly the rules the model receives.
async fn list_instructions(
    cwd: &Path,
    paths: &crate::agent::config::PathsConfig,
    project_trusted: bool,
) -> Vec<InstructionFile> {
    let configs = distill_agent::prompt::agents_md::read_agents_config_with_paths(
        &cwd.display().to_string(),
        distill_agent::prompt::skills::CompatConfig::default(),
        paths,
        project_trusted,
    )
    .await;

    let distill_home = crate::util::distill_home::distill_home();
    let vendor_homes = distill_dirs::home_dir()
        .map(|home_dir| {
            vec![
                (home_dir.join(".claude"), true),
                (home_dir.join(".cursor"), true),
            ]
        })
        .unwrap_or_default();
    let workspace_root = git2::Repository::discover(cwd)
        .ok()
        .and_then(|repo| repo.workdir().map(Path::to_path_buf))
        .unwrap_or_else(|| cwd.to_path_buf());

    // Phase 2 cutoff: when imported, stop classifying `.claude/rules/` paths as rules
    // Equivalent dirs come in via `[paths] extra_rule_dirs`
    let imported = crate::claude_import::is_claude_import_marked();

    configs
        .into_iter()
        .map(|c| {
            let (scope, file_type, vendor) = if c.source
                == distill_agent::prompt::agents_md::InstructionSource::Configured
            {
                (Scope::Global, "rules", None)
            } else {
                (
                    instruction_scope(&c.file_path, &distill_home, &vendor_homes, &workspace_root),
                    instruction_file_type(&c.file_path, &distill_home, imported),
                    derive_vendor(&c.file_path).map(String::from),
                )
            };
            let size = c.content.len();
            InstructionFile {
                size_bytes: size,
                approx_tokens: estimate_tokens(&c.content),
                path: c.file_path,
                scope,
                file_type: file_type.to_string(),
                vendor,
                disabled: false,
                compatibility_status: None,
            }
        })
        .collect()
}

/// The full-lockdown sources (MCP, marketplace): they block everything they
/// bind while listing no entries, so an unnamed lockdown is invisible.
fn policy_lockdown_sources(
    ms: &distill_workspace::permission::resolution::ManagedSettings,
) -> (Vec<LockdownSource>, Vec<LockdownSource>) {
    use distill_workspace::permission::resolution::PolicySourceAuthority;
    let lockdown = |path: Option<&Path>, authority: PolicySourceAuthority| LockdownSource {
        source: path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unknown source)".to_string()),
        advisory: authority == PolicySourceAuthority::Advisory,
    };
    let mcp = ms
        .mcp_allowlist
        .sources
        .iter()
        .filter(|s| s.is_lockdown())
        .map(|s| lockdown(s.source_path.as_deref(), s.authority()))
        .collect();
    let marketplace = ms
        .marketplace_allowlist
        .sources
        .iter()
        .filter(|s| s.is_lockdown())
        .map(|s| lockdown(s.source_path.as_deref(), s.authority))
        .collect();
    (mcp, marketplace)
}

/// Builds the report from the production permission resolver.
async fn list_permissions(cwd: &Path, project_trusted: bool) -> PermissionsReport {
    use distill_workspace::permission::resolution;

    let ms = resolution::managed_settings();
    let format_entry = |e: &resolution::AllowedMcpServer| match e {
        resolution::AllowedMcpServer::Http { url_pattern } => url_pattern.clone(),
        resolution::AllowedMcpServer::Stdio { command } => format!("command:{command}"),
        resolution::AllowedMcpServer::StdioArgv { argv } => {
            format!("command:{}", argv.join(" "))
        }
        resolution::AllowedMcpServer::Name { name } => format!("name:{name}"),
    };
    let mcp_server_allowlist: Vec<String> = ms
        .mcp_allowlist
        .sources
        .iter()
        .flat_map(|source| {
            source.entries().map(format_entry).chain(
                source
                    .deny_entries()
                    .map(|e| format!("deny:{}", format_entry(e))),
            )
        })
        .collect();
    let (mcp_lockdown_sources, marketplace_lockdown_sources) = policy_lockdown_sources(ms);
    let marketplace_allowlist = ms.marketplace_allowlist.allowed_urls();
    let managed_marketplaces: Vec<String> = ms
        .extra_marketplaces
        .iter()
        .map(|m| match &m.kind {
            resolution::ManagedMarketplaceKind::Git { url, git_ref } => match git_ref {
                Some(r) => format!("{} ({url}@{r})", m.name),
                None => format!("{} ({url})", m.name),
            },
            resolution::ManagedMarketplaceKind::Local { path } => format!("{} ({path})", m.name),
        })
        .collect();

    // Independent of the rule resolver: a managed-settings.json containing only
    // e.g. disableBypassPermissionsMode still surfaces its path and effects.
    let managed_settings_path =
        crate::config::claude_managed_settings_probe_path().map(|p| p.display().to_string());
    let managed_settings_exists =
        crate::config::claude_managed_settings_probe_path().is_some_and(|p| p.exists());
    // `source_path` is set only when the file was read and parsed successfully, so it signals "actually loaded" rather than merely present
    let managed_settings_active = ms.features.source_path.is_some();

    // The resolution carries the always-approve pin it applied, even when nothing resolves, so the enforced row and the resolved rules can't disagree about the pin. This covers only the pin: `ms.features` rows come from a separate `managed_settings()` read.
    let resolution::ProvenanceResolution {
        yolo_lock,
        resolved,
    } = resolution::resolve_permissions_with_provenance(cwd, project_trusted).await;
    let (sources, loaded, skipped) = match resolved {
        Some(resolved) => {
            let mut sources: Vec<String> = resolved.sources.iter().map(|s| s.to_string()).collect();
            sources.dedup();
            let skipped = resolved
                .skipped
                .into_iter()
                .map(|s| SkippedRule {
                    rule: s.rule,
                    reason: s.reason,
                })
                .collect();
            (sources, resolved.config.rules.len(), skipped)
        }
        // Nothing resolved: no rules, but the pin above still surfaces as an enforced row.
        None => (vec![], 0, vec![]),
    };
    let PermissionPolicyReport {
        enforced,
        claude_bypass_lock_advisory,
    } = permission_policy_report(ms, yolo_lock.as_ref());

    PermissionsReport {
        sources,
        loaded,
        skipped,
        mcp_server_allowlist,
        mcp_lockdown_sources,
        mcp_managed_servers_only: ManagedOnlyScope::resolve(&ms.mcp_allowlist),
        marketplace_allowlist,
        marketplace_lockdown_sources,
        managed_marketplaces,
        managed_settings_path,
        managed_settings_exists,
        managed_settings_active,
        enforced,
        claude_bypass_lock_advisory,
    }
}

/// Enforced rows plus the Claude bypass-lock advisory flag.
struct PermissionPolicyReport {
    enforced: Vec<EnforcedPolicy>,
    claude_bypass_lock_advisory: bool,
}

/// Build the managed-policy report: one enforced row per active clamp; a
/// Claude bypass-lock request must never appear as an enforced row.
fn permission_policy_report(
    ms: &ManagedSettings,
    yolo_lock: Option<&YoloPolicyLock>,
) -> PermissionPolicyReport {
    let mut enforced = Vec::new();
    if let Some(lock) = yolo_lock {
        enforced.push(EnforcedPolicy {
            setting: EnforcedSetting::AlwaysApprove,
            enabled: false,
            source: lock.source_label.clone(),
        });
    }
    if let Some(src) = &ms.features.source_path {
        // Full path, matching the alwaysApprove row's granularity.
        let source = src.display().to_string();
        for (flag, setting) in [
            (ms.features.disable_telemetry, EnforcedSetting::Telemetry),
            (ms.features.disable_feedback, EnforcedSetting::Feedback),
        ] {
            if flag == Some(true) {
                enforced.push(EnforcedPolicy {
                    setting,
                    enabled: false,
                    source: source.clone(),
                });
            }
        }
    }
    for (pin, setting) in [
        (&ms.project_mcp, EnforcedSetting::ProjectMcpServers),
        (&ms.plugin_auto_update, EnforcedSetting::PluginAutoUpdate),
    ] {
        if let Some(source) = pin.source() {
            enforced.push(EnforcedPolicy {
                setting,
                enabled: false,
                source: source.display().to_string(),
            });
        }
    }
    PermissionPolicyReport {
        enforced,
        claude_bypass_lock_advisory: claude_bypass_lock_request(&ms.features),
    }
}

/// Resolves the enterprise login-hardening knobs from the merged config (`[grok_com_config]`, the `[auth]` alias, and env overrides).
/// Admins use this to confirm the deployment's auth policy actually loaded.
fn login_policy_report(config: Option<&crate::agent::config::Config>) -> LoginPolicyReport {
    let grok_com_config = config
        .map(|c| c.grok_com_config.clone())
        .unwrap_or_default();
    LoginPolicyReport {
        api_key_auth_disabled: grok_com_config.api_key_auth_disabled(),
        disable_api_key_auth: grok_com_config.disable_api_key_auth,
        force_login_team_uuid: grok_com_config.force_login_team_uuid,
    }
}

/// Discovers hooks with every vendor enabled so compatibility can be annotated later.
fn list_hooks(
    git_root: Option<&Path>,
    project_trusted: bool,
    discovered_plugins: &[distill_agent::plugins::DiscoveredPlugin],
) -> Vec<HookEntry> {
    let all_on = distill_tools::types::compat::CompatConfig::default();
    // Route through the same assembly as session startup
    // Config-layer hooks (config.toml / managed_config.toml / requirements.toml) then appear in `/hooks` status alongside file hooks
    // Each carries its provenance name prefix
    let config_layers = distill_config::hook_config_layers();
    let (registry, _errors) =
        crate::util::hooks::assemble_hooks(&config_layers, git_root, &all_on, project_trusted);

    let mut entries: Vec<HookEntry> = registry
        .all_hooks()
        .into_iter()
        .map(|h| {
            // Classify via the shared `hook_origin` (typed provenance and the file-tier name prefix), the same classifier telemetry uses
            // That way admin/system hooks are not mislabeled and inspect cannot diverge from telemetry
            use distill_hooks::config::HookOrigin as O;
            // Config-layer hooks store the layer's directory in `source_dir`; rejoin the tier's filename so inspect shows the actual config file
            let config_file = |name: &str| h.source_dir.join(name);
            let path = h.source_dir.clone();
            let source = match distill_hooks::config::hook_origin(h) {
                O::SystemManaged | O::Managed => ConfigSource::Managed {
                    path: Some(config_file(distill_config::MANAGED_CONFIG_FILENAME)),
                },
                O::Requirements => ConfigSource::Managed {
                    path: Some(config_file(distill_config::REQUIREMENTS_FILENAME)),
                },
                O::UserConfig => ConfigSource::ConfigToml {
                    path: config_file(distill_config::USER_CONFIG_FILENAME),
                },
                O::ProjectFile => ConfigSource::Project { path },
                // File/plugin/agent/unknown hooks are user-scoped for display.
                O::UserFile | O::Plugin | O::Agent | O::Unknown => ConfigSource::User { path },
            };
            let vendor = derive_vendor(&h.source_dir.display().to_string()).map(String::from);
            HookEntry {
                event: h.event.to_string(),
                hook_type: h.handler_type.as_ref().to_string(),
                target: h
                    .command
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .or_else(|| h.url.clone())
                    .unwrap_or_default(),
                source,
                matcher: h.configured_matcher.clone(),
                vendor,
                disabled: false,
                compatibility_status: None,
            }
        })
        .collect();

    // Plugin hooks
    for p in discovered_plugins {
        if !p.trusted {
            continue;
        }
        let source = ConfigSource::Plugin {
            plugin_name: p.manifest.name.clone(),
            path: p.root.clone(),
        };
        if let Some(ref hooks_path) = p.hooks_path {
            entries.push(HookEntry {
                event: "(plugin)".to_string(),
                hook_type: "file".to_string(),
                target: hooks_path.display().to_string(),
                source,
                matcher: None,
                vendor: None,
                disabled: false,
                compatibility_status: None,
            });
        } else if p.manifest.inline_hooks().is_some() {
            entries.push(HookEntry {
                event: "(plugin)".to_string(),
                hook_type: "inline".to_string(),
                target: String::new(),
                source,
                matcher: None,
                vendor: None,
                disabled: false,
                compatibility_status: None,
            });
        }
    }

    entries
}

async fn list_skills(
    cwd: &Path,
    plugin_registry: &distill_agent::plugins::PluginRegistry,
    skills_config: &distill_agent::prompt::skills::SkillsConfig,
    project_trusted: bool,
) -> Vec<SkillEntry> {
    let skills = distill_agent::prompt::skills::list_skills_with_plugins(
        Some(&cwd.display().to_string()),
        skills_config,
        Some(plugin_registry),
        distill_agent::prompt::skills::CompatConfig::default(),
        project_trusted,
    )
    .await;

    let name_counts = slash_name_counts(&skills);
    skills
        .into_iter()
        .map(|s| {
            let source = skill_entry_source(&s);
            let vendor = derive_vendor(&s.path).map(String::from);
            let (collides_with, invocable_as) = slash_collision(&s, &name_counts);
            SkillEntry {
                name: s.label().to_string(),
                description: s.description,
                source,
                user_invocable: s.user_invocable,
                vendor,
                // Preserve `[skills].disabled`; compatibility is applied later.
                disabled: !s.enabled,
                compatibility_status: None,
                collides_with,
                invocable_as,
            }
        })
        .collect()
}

fn slash_name_counts(
    skills: &[distill_agent::prompt::skills::SkillInfo],
) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for skill in skills.iter().filter(|s| s.user_invocable && s.enabled) {
        *counts.entry(skill.name.to_lowercase()).or_default() += 1;
        *counts
            .entry(
                distill_tools::implementations::skills::skill::format_skill_name(skill)
                    .to_lowercase(),
            )
            .or_default() += 1;
    }
    counts
}

fn slash_collision(
    skill: &distill_agent::prompt::skills::SkillInfo,
    name_counts: &HashMap<String, usize>,
) -> (Option<String>, Option<String>) {
    if !skill.user_invocable || !skill.enabled {
        return (None, None);
    }
    let name_key = skill.name.to_lowercase();
    let contested = crate::session::slash_commands::is_reserved_slash_name(&skill.name)
        || name_counts.get(&name_key).is_some_and(|n| *n > 1);
    if !contested {
        return (None, None);
    }
    let qualified =
        distill_tools::implementations::skills::skill::format_skill_name(skill).to_lowercase();
    let invocable_as = name_counts
        .get(&qualified)
        .is_some_and(|n| *n == 1)
        .then_some(qualified);
    (Some(skill.name.clone()), invocable_as)
}

/// Prefers the `config_source` stamped at discovery (plugin skills, `[skills].paths` entries), then falls back to the discovered scope.
fn skill_entry_source(s: &distill_agent::prompt::skills::SkillInfo) -> ConfigSource {
    use distill_tools::implementations::skills::types::SkillScope;

    if let Some(source) = s.config_source.clone() {
        return source;
    }
    let path = PathBuf::from(&s.path);
    match s.scope {
        SkillScope::Local | SkillScope::Repo => ConfigSource::Project { path },
        SkillScope::User => ConfigSource::User { path },
        SkillScope::Server => ConfigSource::Server { path },
        SkillScope::Bundled => ConfigSource::Bundled { path },
        SkillScope::Plugin => ConfigSource::Plugin {
            plugin_name: String::new(),
            path,
        },
    }
}

fn list_agents(
    cwd: &Path,
    plugin_registry: &distill_agent::plugins::PluginRegistry,
) -> Vec<AgentEntry> {
    let agents = distill_agent::discovery::all_subagents_with_plugins(
        cwd,
        &HashMap::new(),
        Some(plugin_registry),
    );

    agents
        .into_iter()
        .map(|a| AgentEntry {
            name: a.name,
            description: a.description,
            source: a.config_source,
        })
        .collect()
}

/// Maps pre-discovered plugins (from `discover_plugins`) to inspect entries.
fn list_plugins(discovered: &[distill_agent::plugins::DiscoveredPlugin]) -> Vec<PluginEntry> {
    discovered
        .iter()
        .map(|p| {
            let scope = match p.scope {
                distill_agent::plugins::PluginScope::CliOverride => Scope::Cli,
                distill_agent::plugins::PluginScope::Project => Scope::Project,
                distill_agent::plugins::PluginScope::User => Scope::User,
                distill_agent::plugins::PluginScope::ConfigPath => Scope::Config,
            };
            PluginEntry {
                name: p.manifest.name.clone(),
                scope,
                path: p.root.display().to_string(),
                enabled: p.trusted,
                provides: PluginProvides {
                    // Count the SKILL.md files discovered (root-level or in subdirs), not the number of configured skill dirs
                    // The reported count then matches what the skills registry loads
                    skills: distill_agent::plugins::registry::skill_md_paths(&p.skill_dirs).len(),
                    agents: p.agent_dirs.len(),
                    hooks: p.hooks_path.is_some(),
                    mcp_servers: if p.mcp_config_path.is_some() { 1 } else { 0 },
                },
            }
        })
        .collect()
}

/// Wraps the production marketplace resolver (`marketplace::resolve`).
fn list_marketplaces(git_root: Option<&Path>) -> Vec<MarketplaceEntry> {
    let Some(root) = git_root else {
        return vec![];
    };
    distill_agent::plugins::marketplace::resolve(root)
        .into_iter()
        .map(|m| MarketplaceEntry {
            name: m.name,
            path: m.path.display().to_string(),
            enabled_plugins: m.plugin_dirs.len(),
        })
        .collect()
}

/// Discovers MCPs with every vendor enabled so compatibility can be annotated later.
fn list_mcp_servers(
    cwd: &Path,
    plugin_registry: &distill_agent::plugins::PluginRegistry,
) -> Vec<McpServerEntry> {
    use distill_workspace::permission::resolution;

    let all_on = distill_tools::types::compat::CompatConfig::default();
    let sourced = crate::session::managed_mcp::merge_managed_mcp_servers_sourced(
        cwd,
        Some(plugin_registry),
        &all_on,
    );
    let ms = resolution::managed_settings();
    let project = crate::agent::folder_trust::project_scoped_mcp_names(cwd);

    sourced
        .into_iter()
        .map(|(server, source)| {
            let (name, transport, target) =
                match &server {
                    agent_client_protocol::McpServer::Stdio(
                        agent_client_protocol::McpServerStdio { name, command, .. },
                    ) => (name.clone(), "stdio", command.display().to_string()),
                    agent_client_protocol::McpServer::Http(
                        agent_client_protocol::McpServerHttp { name, url, .. },
                    ) => (name.clone(), "http", url.clone()),
                    agent_client_protocol::McpServer::Sse(
                        agent_client_protocol::McpServerSse { name, url, .. },
                    ) => (name.clone(), "sse", url.clone()),
                    // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
                    _ => ("unknown".to_string(), "unknown", String::new()),
                };
            let subject = crate::session::managed_mcp::mcp_subject(&server, &source, &project);
            // The verdict mirrors the merge's deny/allow and project-MCP pin
            // so the report matches what actually loads.
            let disabled_reason = match ms.mcp_verdict(&server, subject) {
                resolution::McpVerdict::Blocked(reason) => Some(reason.to_string()),
                resolution::McpVerdict::Allowed => None,
            };
            let vendor = match &source {
                ConfigSource::ClaudeJson { .. } => Some("claude".to_owned()),
                ConfigSource::McpJson { path } => {
                    derive_vendor(&path.display().to_string()).map(String::from)
                }
                _ => None,
            };
            McpServerEntry {
                name,
                transport: transport.to_string(),
                target,
                source,
                disabled: false,
                compatibility_status: None,
                disabled_reason,
                vendor,
            }
        })
        .collect()
}

/// Wraps the production LSP loader (`load_servers_with_plugins_sourced`).
fn list_lsp_servers(
    cwd: &Path,
    discovered_plugins: &[distill_agent::plugins::DiscoveredPlugin],
) -> Vec<LspServerEntry> {
    let trusted: Vec<_> = discovered_plugins.iter().filter(|p| p.trusted).collect();
    let plugin_lsp_paths: Vec<std::path::PathBuf> = trusted
        .iter()
        .filter_map(|p| p.lsp_config_path.clone())
        .collect();
    let plugin_names: Vec<&str> = trusted
        .iter()
        .filter(|p| p.lsp_config_path.is_some())
        .map(|p| p.manifest.name.as_str())
        .collect();
    let plugin_inline_lsp: Vec<(&serde_json::Value, &str)> = trusted
        .iter()
        .filter_map(|p| {
            p.manifest
                .inline_lsp_servers()
                .map(|v| (v, p.manifest.name.as_str()))
        })
        .collect();
    let inline_values: Vec<&serde_json::Value> =
        plugin_inline_lsp.iter().map(|(v, _)| *v).collect();
    let inline_names: Vec<&str> = plugin_inline_lsp.iter().map(|(_, n)| *n).collect();

    let servers = distill_tools::implementations::lsp::config::load_servers_with_plugins_sourced(
        cwd,
        &plugin_lsp_paths,
        &inline_values,
        &plugin_names,
        &inline_names,
    );

    // Folder-trust gate, display-only: inspect never spawns servers
    // Mark the repo-local (project-scoped) entries a session would skip in an untrusted clone, so the listing matches the live gate
    // `remote = None` mirrors `grok mcp doctor` (no loaded RemoteSettings in a standalone command)
    crate::agent::folder_trust::resolve_and_record(cwd, None, false);
    let project_allowed = crate::agent::folder_trust::project_scope_allowed(cwd);

    servers
        .into_iter()
        .map(|(name, (cfg, source))| {
            let untrusted = !project_allowed && matches!(source, ConfigSource::Project { .. });
            LspServerEntry {
                name,
                command: cfg.command,
                args: cfg.args,
                source,
                extensions: cfg.extensions.keys().cloned().collect(),
                untrusted,
            }
        })
        .collect()
}

/// Locates the config files that contribute to the effective config, probing the locations `ConfigLayers::load` and `requirements_layers` use.
/// Probed: system and user `managed_config.toml`, user `config.toml`, and user and system `requirements.toml`.
/// The macOS MDM managed-preferences layer has no file on disk, so it is sourced directly from `requirements_layers()` rather than a path probe. Only on-disk files (plus the synthetic MDM layer) are emitted. `note` distinguishes files that exist but contribute nothing after the real loader's processing (stripping, version overrides, fail_closed, etc).
fn list_config_sources(cwd: &Path) -> ConfigSources {
    let mut layers: Vec<ConfigLayer> = vec![];

    // System managed (comes first in merge precedence)
    if let Some(dir) = crate::config::system_config_dir() {
        let p = dir.join("managed_config.toml");
        if let Some((path_s, note)) = describe_config_file(&p) {
            layers.push(ConfigLayer {
                role: "system-managed".to_string(),
                path: path_s,
                note,
            });
        }
    }

    // User managed
    if let Some(home) = crate::config::user_distill_home() {
        let p = home.join("managed_config.toml");
        if let Some((path_s, note)) = describe_config_file(&p) {
            layers.push(ConfigLayer {
                role: "managed".to_string(),
                path: path_s,
                note,
            });
        }
    }

    // User config.toml (primary user layer; shown as (none) when absent)
    if let Some(home) = crate::config::user_distill_home() {
        let p = home.join("config.toml");
        if let Some((path_s, note)) = describe_config_file(&p) {
            layers.push(ConfigLayer {
                role: "user".to_string(),
                path: path_s,
                note,
            });
        }
    }

    let inline_env = crate::config::GROK_CONFIG_ENV;
    let path_env = crate::config::GROK_CONFIG_PATH_ENV;
    if let Some(overlay) = crate::config::resolved_env_overlay() {
        if !overlay.sections.is_empty() {
            let path = match overlay.source {
                crate::config::OverlaySource::Inline => format!("${inline_env} (inline)"),
                crate::config::OverlaySource::Path(p) => p.display().to_string(),
            };
            layers.push(ConfigLayer {
                role: "env_overlay".to_string(),
                path,
                note: Some(format!("sections: {}", overlay.sections.join(", "))),
            });
        }
    } else if std::env::var_os(inline_env).is_some_and(|v| !v.to_string_lossy().trim().is_empty())
        || std::env::var_os(path_env).is_some_and(|v| !v.is_empty())
    {
        layers.push(ConfigLayer {
            role: "env_overlay".to_string(),
            path: format!("${inline_env} / ${path_env}"),
            note: Some("set but ignored (empty, malformed, or unreadable)".to_string()),
        });
    }

    // Requirements: user then system (order they appear in requirements_layers)
    if let Some(home) = crate::config::user_distill_home() {
        let p = home.join("requirements.toml");
        if let Some((path_s, note)) = describe_requirements_file(&p) {
            layers.push(ConfigLayer {
                role: "requirements".to_string(),
                path: path_s,
                note,
            });
        }
    }
    if let Some(dir) = crate::config::system_config_dir() {
        let p = dir.join("requirements.toml");
        if let Some((path_s, note)) = describe_requirements_file(&p) {
            layers.push(ConfigLayer {
                role: "system-requirements".to_string(),
                path: path_s,
                note,
            });
        }
    }

    // macOS MDM managed preferences: a synthetic, admin-forced requirements layer with no file on disk
    // It comes from requirements_layers() (keyed on the synthetic label), and the in-memory value decides contribution rather than a path probe
    // Absent on non-macOS or when no profile is forced
    let rt_layers = crate::config::requirements_layers();
    if let Some(mdm) = rt_layers
        .iter()
        .find(|l| matches!(l.source, crate::config::RequirementsSource::Mdm))
    {
        let path_s = mdm.source.label().into_owned();
        let note = if requirements_layer_contributes(&rt_layers, &path_s) {
            None
        } else {
            Some("empty".to_string())
        };
        layers.push(ConfigLayer {
            role: "mdm".to_string(),
            path: path_s,
            note,
        });
    }

    // Project configs (from git root up); each is its own "project" role entry
    for p in crate::config::find_project_configs(cwd) {
        if p.exists()
            && let Some((path_s, note)) = describe_config_file(&p)
        {
            layers.push(ConfigLayer {
                role: "project".to_string(),
                path: path_s,
                note,
            });
        }
    }

    ConfigSources { layers }
}

/// For managed / user / project config files, use `load_config_file`, the production path for those layers.
/// `note` then reflects post-processing (version overrides stripped) and distinguishes parse failure.
fn describe_config_file(path: &Path) -> Option<(String, Option<String>)> {
    if !path.exists() {
        return None;
    }
    let path_s = path.display().to_string();
    match crate::config::load_config_file(path) {
        Ok(v) => {
            let empty = v.as_table().is_none_or(|t| t.is_empty());
            Some((
                path_s,
                if empty {
                    Some("empty".to_string())
                } else {
                    None
                },
            ))
        }
        Err(_) => Some((path_s, Some("parse error".to_string()))),
    }
}

/// Classify a requirements file against the real loader.
/// `load_config_file` catches syntax errors and invalid `[[version_overrides]]` (the loader rejects the latter too), so those read "(parse error)".
/// Contribution then comes from `requirements_layers()` via `requirements_layer_contributes`.
fn describe_requirements_file(path: &Path) -> Option<(String, Option<String>)> {
    if !path.exists() {
        return None;
    }
    let path_s = path.display().to_string();
    if crate::config::load_config_file(path).is_err() {
        return Some((path_s, Some("parse error".to_string())));
    }
    if requirements_layer_contributes(&crate::config::requirements_layers(), &path_s) {
        Some((path_s, None))
    } else {
        Some((path_s, Some("empty".to_string())))
    }
}

/// Whether the loader keeps `path_s` *and* its post-load table is non-empty.
/// The non-empty guard runs before `fail_closed` is stripped, so a `fail_closed`-only file is retained with an empty table yet contributes nothing.
fn requirements_layer_contributes(
    layers: &[crate::config::RequirementsLayer],
    path_s: &str,
) -> bool {
    layers.iter().any(|l| {
        l.source.label().as_ref() == path_s && l.value.as_table().is_some_and(|t| !t.is_empty())
    })
}

fn print_section<T>(
    out: &mut impl Write,
    title: &str,
    items: &[T],
    format_item: impl Fn(&T) -> String,
) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  {} ({})", title, items.len())?;
    if items.is_empty() {
        writeln!(out, "  {TREE} (none)")?;
    }
    for item in items {
        writeln!(out, "  {TREE} {}", format_item(item))?;
    }
    Ok(())
}

/// Print items in a two-column layout: name on the left, source label on the right.
fn print_columns<T>(
    out: &mut impl Write,
    title: &str,
    items: &[T],
    name: impl Fn(&T) -> String,
    label: impl Fn(&T) -> String,
) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  {} ({})", title, items.len())?;
    if items.is_empty() {
        writeln!(out, "  {TREE} (none)")?;
        return Ok(());
    }
    let names: Vec<String> = items.iter().map(&name).collect();
    let pad = names.iter().map(String::len).max().unwrap_or(0).min(50);
    for (item, n) in items.iter().zip(&names) {
        writeln!(out, "  {TREE} {:<pad$}  {}", n, label(item))?;
    }
    Ok(())
}

/// Render the team pin for the human view: single value, comma-joined list, or an explicit empty-list marker (which fails closed at login).
fn format_force_login_team(team: &Option<ForceLoginTeam>) -> String {
    match team {
        None => "(none)".to_string(),
        Some(ForceLoginTeam::Single(s)) => s.clone(),
        Some(ForceLoginTeam::AnyOf(list)) if list.is_empty() => {
            "(empty -- fail closed)".to_string()
        }
        Some(ForceLoginTeam::AnyOf(list)) => list.join(", "),
    }
}

/// Human label for an enforced setting.
/// Uses product vocabulary, not the internal field names (no `ui.yolo` / `--yolo` / `permission_mode`).
fn enforced_label(p: &EnforcedPolicy) -> String {
    let name = match p.setting {
        EnforcedSetting::AlwaysApprove => "Permissions mode: always-approve",
        EnforcedSetting::Telemetry => "Telemetry",
        EnforcedSetting::Feedback => "Feedback",
        EnforcedSetting::ProjectMcpServers => "Project MCP servers",
        EnforcedSetting::PluginAutoUpdate => "Plugin auto-update",
    };
    let state = if p.enabled { "enabled" } else { "disabled" };
    format!("{name} {state}")
}

/// Human-mode advisory line for a Claude bypass-lock request; layout belongs
/// to the caller.
fn claude_bypass_advisory_message(p: &PermissionsReport) -> Option<&'static str> {
    p.claude_bypass_lock_advisory.then_some(
        "Claude disableBypassPermissionsMode: advisory only -- not enforced for grok \
         (lock via requirements.toml [ui] disable_bypass_permissions_mode)",
    )
}

fn disabled_compat_tags(
    disabled: bool,
    compatibility_status: Option<CompatEntryStatus>,
) -> &'static str {
    if disabled || compatibility_status == Some(CompatEntryStatus::Disabled) {
        " [disabled]"
    } else {
        ""
    }
}

fn render_config_warnings(
    warnings: &[crate::agent::config_model_override_parse::ConfigWarning],
) -> String {
    use std::fmt::Write as _;

    if warnings.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n  Config Warnings\n");
    let _ = writeln!(out, "  {TREE} {} warning(s)", warnings.len());
    for w in warnings {
        let field = w.field().map(|f| format!(" {f}")).unwrap_or_default();
        let _ = writeln!(
            out,
            "    {TREE} [{}]{field} — {}",
            w.target.label(),
            w.reason
        );
    }
    out
}

fn render_mcp_config_problems(problems: &[crate::util::config::McpServerConfigProblem]) -> String {
    use crate::util::config::McpServerProblemSeverity;
    use std::fmt::Write as _;

    if problems.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n  MCP Config Problems\n");
    let _ = writeln!(out, "  {TREE} {} problem(s)", problems.len());
    for p in problems {
        let severity = match p.severity {
            McpServerProblemSeverity::Error => "error",
            McpServerProblemSeverity::Warning => "warning",
        };
        let _ = writeln!(out, "    {TREE} [{severity}] {}", p.message);
    }
    out
}

fn render_harness_compatibility(report: &ExternalCompatReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("\n  Harness Compatibility\n");
    let mut current_vendor = "";
    for cell in &report.cells {
        if cell.vendor != current_vendor {
            current_vendor = &cell.vendor;
            let _ = writeln!(out, "  {TREE} {current_vendor}");
        }
        let status = if cell.enabled { "on" } else { "OFF" };
        let _ = writeln!(
            out,
            "    {TREE} {:<10} {:<3}  ({})",
            cell.surface, status, cell.source
        );
    }
    out.push('\n');
    out
}

fn print_human(r: &InspectReport, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(out, "  Environment")?;
    writeln!(out, "  {TREE} Version: {} [{}]", r.grok_version, r.channel)?;
    writeln!(out, "  {TREE} CWD: {}", r.cwd)?;
    if let Some(ref root) = r.project_root {
        writeln!(out, "  {TREE} Git root: {}", root)?;
    }
    writeln!(
        out,
        "  {TREE} Project trusted: {}",
        if r.project_trusted { "yes" } else { "no" }
    )?;

    print_section(out, "Project Instructions", &r.project_instructions, |f| {
        let status = disabled_compat_tags(f.disabled, f.compatibility_status);
        format!(
            "{} ({}, ~{} tokens){}{}",
            f.path,
            f.scope,
            f.approx_tokens,
            vendor_tag(&f.vendor),
            status,
        )
    })?;

    writeln!(out)?;
    writeln!(out, "  Permissions")?;
    if r.permissions.managed_settings_exists
        && let Some(ref p) = r.permissions.managed_settings_path
    {
        let status = if r.permissions.managed_settings_active {
            "active"
        } else {
            "not loaded"
        };
        writeln!(out, "  {TREE} Managed settings: {p} ({status})")?;
    }
    if r.permissions.sources.is_empty() {
        writeln!(out, "  {TREE} Source: (none)")?;
    } else {
        for src in &r.permissions.sources {
            writeln!(out, "  {TREE} Source: {src}")?;
        }
    }
    writeln!(
        out,
        "  {TREE} {} loaded, {} skipped",
        r.permissions.loaded,
        r.permissions.skipped.len()
    )?;
    for s in &r.permissions.skipped {
        writeln!(out, "    {TREE} {} -- {}", s.rule, s.reason)?;
    }
    if !r.permissions.enforced.is_empty() {
        writeln!(out, "  {TREE} Enforced by policy")?;
        for e in &r.permissions.enforced {
            writeln!(out, "    {TREE} {} ({})", enforced_label(e), e.source)?;
        }
    }
    if let Some(msg) = claude_bypass_advisory_message(&r.permissions) {
        writeln!(out, "  {TREE} {msg}")?;
    }
    if !r.permissions.mcp_server_allowlist.is_empty() {
        writeln!(
            out,
            "  {TREE} MCP server allowlist ({} patterns)",
            r.permissions.mcp_server_allowlist.len()
        )?;
        for pat in &r.permissions.mcp_server_allowlist {
            writeln!(out, "    {TREE} {}", pat)?;
        }
    }
    if !r.permissions.mcp_lockdown_sources.is_empty() {
        writeln!(
            out,
            "  {TREE} MCP servers locked down (empty/malformed allowedMcpServers or malformed deniedMcpServers)"
        )?;
        for src in &r.permissions.mcp_lockdown_sources {
            writeln!(out, "    {TREE} {}", src.human_row("servers"))?;
        }
    }
    match r.permissions.mcp_managed_servers_only {
        ManagedOnlyScope::Off => {}
        ManagedOnlyScope::Enforced => writeln!(
            out,
            "  {TREE} MCP managed servers only: enforced (allowManagedMcpServersOnly)"
        )?,
        ManagedOnlyScope::Advisory => writeln!(
            out,
            "  {TREE} MCP managed servers only: advisory (Claude managed-settings; grok-native servers exempt)"
        )?,
    }
    if !r.permissions.marketplace_allowlist.is_empty() {
        writeln!(
            out,
            "  {TREE} Marketplace allowlist ({} sources)",
            r.permissions.marketplace_allowlist.len()
        )?;
        for url in &r.permissions.marketplace_allowlist {
            writeln!(out, "    {TREE} {}", url)?;
        }
    }
    if !r.permissions.marketplace_lockdown_sources.is_empty() {
        writeln!(
            out,
            "  {TREE} Marketplaces locked down (empty or malformed strictKnownMarketplaces)"
        )?;
        for src in &r.permissions.marketplace_lockdown_sources {
            writeln!(out, "    {TREE} {}", src.human_row("marketplaces"))?;
        }
    }
    if !r.permissions.managed_marketplaces.is_empty() {
        writeln!(
            out,
            "  {TREE} Managed marketplaces ({} pinned)",
            r.permissions.managed_marketplaces.len()
        )?;
        for m in &r.permissions.managed_marketplaces {
            writeln!(out, "    {TREE} {}", m)?;
        }
    }

    writeln!(out)?;
    writeln!(out, "  Login Policy")?;
    writeln!(
        out,
        "  {TREE} disable_api_key_auth: {}",
        match r.login_policy.disable_api_key_auth {
            Some(v) => v.to_string(),
            None => "(unset)".to_string(),
        }
    )?;
    writeln!(
        out,
        "  {TREE} force_login_team_uuid: {}",
        format_force_login_team(&r.login_policy.force_login_team_uuid)
    )?;
    writeln!(
        out,
        "  {TREE} api_key_auth_disabled: {}",
        r.login_policy.api_key_auth_disabled
    )?;

    print_columns(
        out,
        "Skills",
        &r.skills,
        |s| s.name.clone(),
        |s| {
            let status = disabled_compat_tags(s.disabled, s.compatibility_status);
            let collision = match (&s.collides_with, &s.invocable_as) {
                (Some(contested), Some(invocable)) => {
                    format!(" [collides with /{contested} → /{invocable}]")
                }
                (Some(contested), None) => {
                    format!(" [collides with /{contested} — not invocable]")
                }
                _ => String::new(),
            };
            format!(
                "{}{}{}{}",
                s.source.display_label(),
                vendor_tag(&s.vendor),
                status,
                collision,
            )
        },
    )?;

    print_columns(
        out,
        "Agents",
        &r.agents,
        |a| a.name.clone(),
        |a| a.source.display_label(),
    )?;

    print_columns(
        out,
        "Plugins",
        &r.plugins,
        |p| {
            let status = if p.enabled { "enabled" } else { "disabled" };
            format!("{} ({}, {})", p.name, p.scope, status)
        },
        |p| {
            let mut parts = Vec::new();
            if p.provides.skills > 0 {
                parts.push(format!("{} skills", p.provides.skills));
            }
            if p.provides.agents > 0 {
                parts.push(format!("{} agents", p.provides.agents));
            }
            if p.provides.hooks {
                parts.push("hooks".into());
            }
            if p.provides.mcp_servers > 0 {
                parts.push(format!("{} MCPs", p.provides.mcp_servers));
            }
            if parts.is_empty() {
                "-".into()
            } else {
                parts.join(", ")
            }
        },
    )?;

    print_section(out, "Marketplaces", &r.marketplaces, |m| {
        format!(
            "{} ({}, {} enabled plugins)",
            m.name, m.path, m.enabled_plugins
        )
    })?;

    if r.mcp_servers.is_empty() {
        writeln!(out)?;
        writeln!(out, "  MCP Servers (0)")?;
        writeln!(out, "  {TREE} (none) \u{2014} see `grok mcp add --help`")?;
    } else {
        print_columns(
            out,
            "MCP Servers",
            &r.mcp_servers,
            |m| {
                if let Some(ref reason) = m.disabled_reason {
                    format!("{} ({}) [BLOCKED: {}]", m.name, m.transport, reason)
                } else {
                    format!("{} ({})", m.name, m.transport)
                }
            },
            |m| {
                let status = disabled_compat_tags(m.disabled, m.compatibility_status);
                format!(
                    "{}{}{}",
                    m.source.display_label(),
                    vendor_tag(&m.vendor),
                    status,
                )
            },
        )?;
    }

    print_columns(
        out,
        "LSP Servers",
        &r.lsp_servers,
        |l| format!("{} ({} {})", l.name, l.command, l.args.join(" ")),
        |l| {
            let untrusted = if l.untrusted { " [untrusted]" } else { "" };
            format!("{}{}", l.source.display_label(), untrusted)
        },
    )?;

    print_columns(
        out,
        "Hooks",
        &r.hooks,
        |h| {
            let matcher = h
                .matcher
                .as_ref()
                .map(|m| format!(" matcher={}", m))
                .unwrap_or_default();
            format!("{}{}", h.hook_type, matcher)
        },
        |h| {
            let status = disabled_compat_tags(h.disabled, h.compatibility_status);
            format!(
                "{}{}{}",
                h.source.display_label(),
                vendor_tag(&h.vendor),
                status,
            )
        },
    )?;

    writeln!(out)?;
    writeln!(out, "  Config Sources")?;
    // User is always emitted (with (none) when absent) for the primary user config.
    if let Some(user_l) = r.config_sources.layers.iter().find(|l| l.role == "user") {
        let tag = match user_l.note.as_deref() {
            Some("empty") => " (empty)",
            Some("parse error") => " (parse error)",
            _ => "",
        };
        writeln!(out, "  {TREE} User: {}{}", user_l.path, tag)?;
    } else {
        writeln!(out, "  {TREE} User: (none)")?;
    }
    for layer in &r.config_sources.layers {
        if layer.role == "user" {
            continue;
        }
        let tag = match layer.note.as_deref() {
            Some("empty") => " (empty)",
            Some("parse error") => " (parse error)",
            _ => "",
        };
        let label = match layer.role.as_str() {
            "system-managed" => "System Managed",
            "managed" => "Managed",
            "system-requirements" => "System Requirements",
            "requirements" => "Requirements",
            "mdm" => "MDM Requirements",
            "project" => "Project",
            other => other,
        };
        writeln!(out, "  {TREE} {}: {}{}", label, layer.path, tag)?;
    }
    if !r.config_sources.layers.iter().any(|l| l.role == "project") {
        writeln!(out, "  {TREE} Project: (none)")?;
    }

    write!(out, "{}", render_config_warnings(&r.config_warnings))?;
    write!(
        out,
        "{}",
        render_mcp_config_problems(&r.mcp_config_problems)
    )?;

    write!(out, "{}", render_harness_compatibility(&r.external_compat))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::PathsConfig;
    use distill_agent::prompt::skills::{SkillInfo, SkillsConfig};
    use distill_tools::implementations::skills::types::SkillScope;

    fn paths_config(extra_rule_dir: &Path) -> PathsConfig {
        PathsConfig {
            extra_rule_dirs: vec![extra_rule_dir.to_string_lossy().into_owned()],
            ..Default::default()
        }
    }

    #[test]
    fn harness_compatibility_human_output_stays_compact() {
        let effective_config: toml::Value =
            toml::from_str("[compat.cursor]\nrules = false").unwrap();
        let report = compat::resolve_inspect_compat_with_env(Ok(&effective_config), |_| None);

        let human = render_harness_compatibility(&report);

        assert!(human.contains("skills     on   (default)"), "{human}");
        assert!(human.contains("rules      OFF  (config)"), "{human}");
        assert!(
            !human.contains("Defaults shown; remote may override."),
            "{human}"
        );
        assert!(!human.contains("resolved at session start"), "{human}");
        assert!(!human.contains("unresolved"), "{human}");
        assert!(!human.contains("?"), "{human}");
    }

    #[test]
    fn disabled_entry_status_serializes_and_renders_consistently() {
        let entry = InstructionFile {
            path: "/repo/.cursor/AGENTS.md".to_owned(),
            scope: Scope::Project,
            file_type: "agents_md".to_owned(),
            size_bytes: 10,
            approx_tokens: 3,
            vendor: Some("cursor".to_owned()),
            disabled: false,
            compatibility_status: Some(CompatEntryStatus::Disabled),
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({
                "path": "/repo/.cursor/AGENTS.md",
                "scope": "project",
                "fileType": "agents_md",
                "sizeBytes": 10,
                "approxTokens": 3,
                "vendor": "cursor",
                "compatibilityStatus": "disabled"
            })
        );
        assert_eq!(
            disabled_compat_tags(false, entry.compatibility_status),
            " [disabled]"
        );
    }

    #[test]
    fn managed_only_scope_classifies_each_policy_origin() {
        use distill_workspace::permission::resolution::{
            McpServerAllowlist, McpServerPolicy, PolicySourceAuthority,
        };
        let lockdown = || McpServerAllowlist::new(vec![], vec![], None).with_managed_only();

        let native = McpServerPolicy::single(lockdown());
        assert_eq!(
            ManagedOnlyScope::resolve(&native),
            ManagedOnlyScope::Enforced
        );

        let advisory =
            McpServerPolicy::single(lockdown().with_authority(PolicySourceAuthority::Advisory));
        assert_eq!(
            ManagedOnlyScope::resolve(&advisory),
            ManagedOnlyScope::Advisory
        );

        // A native source alongside an advisory one must classify as enforced.
        let both = McpServerPolicy {
            sources: vec![
                lockdown().with_authority(PolicySourceAuthority::Advisory),
                lockdown(),
            ],
        };
        assert_eq!(ManagedOnlyScope::resolve(&both), ManagedOnlyScope::Enforced);

        let unpinned = McpServerPolicy::single(McpServerAllowlist::new(vec![], vec![], None));
        assert_eq!(ManagedOnlyScope::resolve(&unpinned), ManagedOnlyScope::Off);
    }

    #[test]
    fn permissions_report_policy_fields_serialize_stably() {
        let report = PermissionsReport {
            sources: vec!["managed-settings.json".to_owned()],
            loaded: 0,
            skipped: vec![],
            mcp_server_allowlist: vec![],
            mcp_lockdown_sources: vec![],
            mcp_managed_servers_only: ManagedOnlyScope::Advisory,
            marketplace_allowlist: vec![],
            marketplace_lockdown_sources: vec![],
            managed_marketplaces: vec![],
            managed_settings_path: None,
            managed_settings_exists: true,
            managed_settings_active: false,
            enforced: vec![EnforcedPolicy {
                setting: EnforcedSetting::ProjectMcpServers,
                enabled: false,
                source: "/etc/grok/managed-settings.json".to_owned(),
            }],
            claude_bypass_lock_advisory: true,
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "sources": ["managed-settings.json"],
                "loaded": 0,
                "skipped": [],
                "mcpServerAllowlist": [],
                "mcpLockdownSources": [],
                "mcpManagedServersOnly": "advisory",
                "marketplaceAllowlist": [],
                "marketplaceLockdownSources": [],
                "managedMarketplaces": [],
                "managedSettingsExists": true,
                "managedSettingsActive": false,
                "enforced": [{
                    "setting": "projectMcpServers",
                    "enabled": false,
                    "source": "/etc/grok/managed-settings.json"
                }],
                "claudeBypassLockAdvisory": true
            })
        );
        for (scope, wire) in [
            (ManagedOnlyScope::Off, "off"),
            (ManagedOnlyScope::Advisory, "advisory"),
            (ManagedOnlyScope::Enforced, "enforced"),
        ] {
            assert_eq!(serde_json::to_value(scope).unwrap(), wire);
        }
        assert_eq!(
            serde_json::to_value(EnforcedSetting::PluginAutoUpdate).unwrap(),
            "pluginAutoUpdate"
        );
    }

    #[test]
    fn vendor_rule_paths_select_rules_compatibility_cells() {
        let cell = |vendor: &str, surface: &str, enabled: bool| ExternalCompatEntry {
            vendor: vendor.to_owned(),
            surface: surface.to_owned(),
            enabled,
            source: CompatSource::Config,
        };
        let report = ExternalCompatReport {
            remote_settings_loaded: false,
            cells: vec![
                cell("cursor", "rules", false),
                cell("cursor", "agents", true),
                cell("claude", "rules", false),
                cell("claude", "agents", true),
            ],
        };

        for (vendor, path) in [
            ("cursor", "/repo/.cursor/rules/team.md"),
            ("cursor", r"C:\repo\.cursor\rules\team.md"),
            ("claude", "/repo/.claude/rules/team.md"),
            ("claude", r"C:\repo\.claude\rules\team.md"),
        ] {
            let file_type = instruction_file_type(path, Path::new("/home/user/.grok"), false);
            assert_eq!(file_type, "rules");
            assert_eq!(
                instruction_compat_status(&Some(vendor.to_owned()), file_type, &report),
                Some(CompatEntryStatus::Disabled)
            );
        }

        for path in ["/repo/.grok/rules/team.md", r"C:\repo\.grok\rules\team.md"] {
            assert_eq!(
                instruction_file_type(path, Path::new("/home/user/.grok"), false),
                "rules"
            );
        }
        for path in [
            "/repo/.cursor/rules/team.md",
            r"C:\repo\.cursor\rules\team.md",
        ] {
            assert_eq!(
                instruction_file_type(path, Path::new("/home/user/.grok"), true),
                "rules"
            );
        }
        for path in [
            "/repo/.claude/rules/team.md",
            r"C:\repo\.claude\rules\team.md",
        ] {
            let file_type = instruction_file_type(path, Path::new("/home/user/.grok"), true);
            assert_eq!(file_type, "agents_md");
            assert_eq!(
                instruction_compat_status(&Some("claude".to_owned()), file_type, &report),
                Some(CompatEntryStatus::Enabled)
            );
        }
        for path in [
            "/repo/not.cursor/rules/team.md",
            r"C:\repo\.cursor\ruleset\team.md",
        ] {
            assert_eq!(
                instruction_file_type(path, Path::new("/home/user/.grok"), false),
                "agents_md"
            );
        }
    }

    #[test]
    fn distill_home_nested_in_workspace_keeps_direct_surfaces_global() {
        let distill_home = Path::new("/repo/config");
        let workspace = Path::new("/repo");
        for path in ["/repo/config/AGENTS.md", "/repo/config/rules/global.md"] {
            assert!(matches!(
                instruction_scope(path, distill_home, &[], workspace),
                Scope::Global
            ));
        }
        for path in [
            "/repo/config/.grok/rules/project.md",
            "/repo/config/src/AGENTS.md",
        ] {
            assert!(matches!(
                instruction_scope(path, distill_home, &[], workspace),
                Scope::Project
            ));
        }
    }

    #[test]
    fn vendor_home_nested_in_workspace_keeps_direct_surfaces_global() {
        let vendor_homes = vec![(Path::new("/repo/.claude").to_path_buf(), true)];
        let workspace = Path::new("/repo");
        for path in ["/repo/.claude/rules/global.md", "/repo/.claude/CLAUDE.md"] {
            assert!(matches!(
                instruction_scope(path, Path::new("/other/grok"), &vendor_homes, workspace),
                Scope::Global
            ));
        }
        for path in [
            "/repo/.claude/.claude/rules/project.md",
            "/repo/.claude/src/AGENTS.md",
        ] {
            assert!(matches!(
                instruction_scope(path, Path::new("/other/grok"), &vendor_homes, workspace),
                Scope::Project
            ));
        }
    }

    #[test]
    fn workspace_scope_wins_inside_distill_home() {
        let distill_home = Path::new("/custom/grok");
        let workspace = Path::new("/custom/grok/worktrees/repo");
        for path in [
            "/custom/grok/worktrees/repo/.cursor/rules/project.md",
            "/custom/grok/worktrees/repo/src/AGENTS.md",
        ] {
            assert!(matches!(
                instruction_scope(path, distill_home, &[], workspace),
                Scope::Project
            ));
        }
        assert!(matches!(
            instruction_scope("/custom/grok/rules/global.md", distill_home, &[], workspace,),
            Scope::Global
        ));
    }

    #[test]
    fn custom_distill_home_rules_are_classified_as_rules() {
        assert_eq!(
            instruction_file_type(
                "/custom/config/rules/team.md",
                Path::new("/custom/config"),
                false,
            ),
            "rules"
        );
        assert_eq!(
            instruction_file_type(
                "/custom/config/AGENTS.md",
                Path::new("/custom/config"),
                false,
            ),
            "agents_md"
        );
    }

    #[test]
    fn describe_config_file_flags_empty_and_parse_error() {
        let dir = tempfile::tempdir().unwrap();

        // Missing file: describe returns None (no layer entry).
        let missing = dir.path().join("missing.toml");
        assert!(describe_config_file(&missing).is_none());

        // Comment-only and whitespace-only files parse to an empty table after load.
        let comment_only = dir.path().join("comment.toml");
        std::fs::write(&comment_only, "# nothing enforced here\n").unwrap();
        let (_, note) = describe_config_file(&comment_only).unwrap();
        assert_eq!(note.as_deref(), Some("empty"));

        let blank = dir.path().join("blank.toml");
        std::fs::write(&blank, "\n\n").unwrap();
        let (_, note) = describe_config_file(&blank).unwrap();
        assert_eq!(note.as_deref(), Some("empty"));

        // A file with real content contributes config and has no note.
        let with_content = dir.path().join("content.toml");
        std::fs::write(&with_content, "[telemetry]\nmode = \"disabled\"\n").unwrap();
        let (_, note) = describe_config_file(&with_content).unwrap();
        assert!(note.is_none());

        // Malformed TOML is flagged as parse error (distinct from empty).
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[[[ this is not valid toml").unwrap();
        let (_, note) = describe_config_file(&bad).unwrap();
        assert_eq!(note.as_deref(), Some("parse error"));
    }

    #[test]
    fn describe_requirements_file_flags_invalid_version_overrides_as_parse_error() {
        // Valid TOML but invalid `[[version_overrides]]` is rejected by the real loader, so it must read "parse error", not "empty"
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requirements.toml");
        std::fs::write(&path, "[[version_overrides]]\nminimum_version = \"nope\"\n").unwrap();
        let (_, note) = describe_requirements_file(&path).unwrap();
        assert_eq!(note.as_deref(), Some("parse error"));
    }

    #[test]
    fn requirements_layer_contributes_requires_non_empty_post_strip_table() {
        // A `fail_closed`-only file is kept by the loader but with an empty post-strip table, so it must not count as contributing
        let path = "/home/u/.grok/requirements.toml";
        let layer = |v| crate::config::RequirementsLayer {
            value: v,
            source: crate::config::RequirementsSource::File(std::path::PathBuf::from(path)),
            is_system: false,
        };
        let empty = layer(toml::Value::Table(toml::map::Map::new()));
        assert!(!requirements_layer_contributes(
            std::slice::from_ref(&empty),
            path
        ));

        let mut tbl = toml::map::Map::new();
        tbl.insert("telemetry".into(), toml::Value::Boolean(true));
        let full = layer(toml::Value::Table(tbl));
        assert!(requirements_layer_contributes(
            std::slice::from_ref(&full),
            path
        ));
    }

    #[test]
    fn enforced_label_uses_product_vocabulary() {
        let p = EnforcedPolicy {
            setting: EnforcedSetting::AlwaysApprove,
            enabled: false,
            source: "requirements.toml".into(),
        };
        assert_eq!(
            enforced_label(&p),
            "Permissions mode: always-approve disabled"
        );
        assert!(!enforced_label(&p).contains("yolo"));
    }

    fn permissions_report(
        claude_bypass_lock_advisory: bool,
        enforced: Vec<EnforcedPolicy>,
    ) -> PermissionsReport {
        PermissionsReport {
            sources: vec![],
            loaded: 0,
            skipped: vec![],
            mcp_server_allowlist: vec![],
            mcp_lockdown_sources: vec![],
            mcp_managed_servers_only: ManagedOnlyScope::Off,
            marketplace_allowlist: vec![],
            marketplace_lockdown_sources: vec![],
            managed_marketplaces: vec![],
            managed_settings_path: None,
            managed_settings_exists: false,
            managed_settings_active: false,
            enforced,
            claude_bypass_lock_advisory,
        }
    }

    /// Wire contract admins verify against (25-enterprise.md): the advisory
    /// key is always emitted; enforced rows keep camelCase setting keys.
    #[test]
    fn permissions_report_pins_advisory_and_enforced_wire_contract() {
        let json = serde_json::to_value(permissions_report(false, vec![])).unwrap();
        assert_eq!(
            json.get("claudeBypassLockAdvisory"),
            Some(&serde_json::json!(false))
        );
        assert!(
            json.get("enforced").is_none(),
            "empty enforced list must be omitted: {json}"
        );

        let row = EnforcedPolicy {
            setting: EnforcedSetting::AlwaysApprove,
            enabled: false,
            source: "/etc/grok/requirements.toml".to_string(),
        };
        let json = serde_json::to_value(permissions_report(true, vec![row])).unwrap();
        assert_eq!(
            json.get("claudeBypassLockAdvisory"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            json.get("enforced"),
            Some(&serde_json::json!([{
                "setting": "alwaysApprove",
                "enabled": false,
                "source": "/etc/grok/requirements.toml",
            }]))
        );
    }

    #[test]
    fn claude_bypass_advisory_line_gated_on_flag() {
        let msg = claude_bypass_advisory_message(&permissions_report(true, vec![]))
            .expect("advisory flag must surface the message");
        assert!(
            msg.contains("Claude disableBypassPermissionsMode: advisory only"),
            "{msg}"
        );
        assert_eq!(
            claude_bypass_advisory_message(&permissions_report(false, vec![])),
            None
        );
    }

    fn managed_features(
        disable_yolo: Option<bool>,
        disable_telemetry: Option<bool>,
        disable_feedback: Option<bool>,
    ) -> ManagedSettings {
        let mut ms = ManagedSettings::default();
        ms.features = distill_workspace::permission::resolution::ManagedSettingsFeatures {
            disable_telemetry,
            disable_feedback,
            disable_yolo,
            source_path: Some(std::path::PathBuf::from(
                "/etc/claude-code/managed-settings.json",
            )),
        };
        ms
    }

    /// A Claude bypass-lock request surfaces as the advisory flag, never as an
    /// enforced row.
    #[test]
    fn claude_bypass_lock_is_advisory_not_enforced() {
        let PermissionPolicyReport {
            enforced,
            claude_bypass_lock_advisory: advisory,
        } = permission_policy_report(&managed_features(Some(true), None, None), None);
        assert!(advisory);
        assert!(
            enforced.is_empty(),
            "Claude bypass lock must not be reported as enforced: {enforced:?}"
        );

        // Managed telemetry/feedback clamps ARE enforced and keep their rows.
        let PermissionPolicyReport {
            enforced,
            claude_bypass_lock_advisory: advisory,
        } = permission_policy_report(&managed_features(Some(true), Some(true), Some(true)), None);
        assert!(advisory);
        let [tel, fb] = enforced.as_slice() else {
            panic!("expected telemetry and feedback rows: {enforced:?}");
        };
        assert_eq!(tel.setting, EnforcedSetting::Telemetry);
        assert_eq!(fb.setting, EnforcedSetting::Feedback);
        // Same granularity as the alwaysApprove row: the full file path.
        assert_eq!(tel.source, "/etc/claude-code/managed-settings.json");
        assert_eq!(fb.source, "/etc/claude-code/managed-settings.json");
    }

    /// The enforced alwaysApprove row comes from grok's own requirements lock,
    /// attributed to the pinning layer, independent of any Claude
    /// managed-settings file.
    #[test]
    fn requirements_lock_reports_always_approve_enforced() {
        let lock = distill_workspace::permission::resolution::YoloPolicyLock {
            source_label: "/etc/grok/requirements.toml".to_string(),
            reason: distill_workspace::permission::resolution::YoloPinReason::DisableBypassPermissionsMode,
        };
        let PermissionPolicyReport {
            enforced,
            claude_bypass_lock_advisory: advisory,
        } = permission_policy_report(&Default::default(), Some(&lock));
        assert!(!advisory);
        let [row] = enforced.as_slice() else {
            panic!("expected one alwaysApprove row: {enforced:?}");
        };
        assert_eq!(row.setting, EnforcedSetting::AlwaysApprove);
        assert!(!row.enabled);
        assert_eq!(row.source, "/etc/grok/requirements.toml");

        // Both present: the real lock row + the advisory flag, no duplicate row.
        let PermissionPolicyReport {
            enforced,
            claude_bypass_lock_advisory: advisory,
        } = permission_policy_report(&managed_features(Some(true), None, None), Some(&lock));
        assert!(advisory);
        let [row] = enforced.as_slice() else {
            panic!("expected one alwaysApprove row: {enforced:?}");
        };
        assert_eq!(row.source, "/etc/grok/requirements.toml");
    }

    /// An MDM-only lockdown has no requirements.toml; the enforced row must
    /// carry the MDM layer's label.
    #[test]
    fn mdm_pin_attributes_enforced_row_to_mdm_layer() {
        let lock = distill_workspace::permission::resolution::YoloPolicyLock {
            source_label: crate::config::MDM_REQUIREMENTS_SOURCE.to_string(),
            reason: distill_workspace::permission::resolution::YoloPinReason::DisableBypassPermissionsMode,
        };

        let PermissionPolicyReport {
            enforced,
            claude_bypass_lock_advisory: advisory,
        } = permission_policy_report(&Default::default(), Some(&lock));
        assert!(!advisory);
        let [row] = enforced.as_slice() else {
            panic!("expected one alwaysApprove row: {enforced:?}");
        };
        assert_eq!(row.setting, EnforcedSetting::AlwaysApprove);
        assert_eq!(row.source, crate::config::MDM_REQUIREMENTS_SOURCE);
    }

    /// The pins report as enforced rows with stable camelCase setting keys.
    #[test]
    fn managed_pins_report_enforced_rows_with_sources() {
        use distill_workspace::permission::resolution::{PolicyLayerOwnership, PolicyPin};
        let mut ms = ManagedSettings::default();
        ms.project_mcp = PolicyPin::Disabled {
            source: "/etc/grok/managed_config.toml".into(),
            ownership: PolicyLayerOwnership::Admin,
        };
        ms.plugin_auto_update = PolicyPin::Disabled {
            source: "/etc/grok/requirements.toml".into(),
            ownership: PolicyLayerOwnership::Admin,
        };
        let PermissionPolicyReport { enforced, .. } = permission_policy_report(&ms, None);
        assert_eq!(
            serde_json::to_value(&enforced).unwrap(),
            serde_json::json!([
                {
                    "setting": "projectMcpServers",
                    "enabled": false,
                    "source": "/etc/grok/managed_config.toml",
                },
                {
                    "setting": "pluginAutoUpdate",
                    "enabled": false,
                    "source": "/etc/grok/requirements.toml",
                },
            ])
        );

        // Unpinned settings produce no row.
        let PermissionPolicyReport { enforced, .. } =
            permission_policy_report(&ManagedSettings::default(), None);
        assert!(enforced.is_empty(), "{enforced:?}");
    }

    /// Model-override warnings flow from an effective config through `Config` to the human renderer and the JSON report.
    #[test]
    fn config_warnings_inspect_smoke() {
        let effective: toml::Value = toml::from_str(
            r#"
            [model."grok-4.5"]
            model = "grok-4.5"
            env_key = "ANTHROPIC_AUTH_TOKEN"
            compactions_remaining = 1
            send_compactions_remaining = true
            reasoning_effort = "not-a-level"
            "#,
        )
        .unwrap();
        let cfg = crate::agent::config::Config::new_from_toml_cfg(&effective).unwrap();
        let warnings = cfg.config_warnings;
        assert!(
            warnings
                .iter()
                .any(|w| w.field() == Some("send_compactions_remaining")),
            "duplicate alias should warn: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.field() == Some("reasoning_effort")),
            "invalid enum should warn: {warnings:?}"
        );
        assert!(cfg.config_models.contains_key("grok-4.5"));

        let human = render_config_warnings(&warnings);
        assert!(human.contains("Config Warnings"), "{human}");
        assert!(
            human.contains("[model.\"grok-4.5\"] send_compactions_remaining"),
            "{human}"
        );
        assert!(
            human.contains("[model.\"grok-4.5\"] reasoning_effort"),
            "{human}"
        );
        // Auth-provider warnings render under their own table syntax.
        let provider_warning =
            crate::agent::config_model_override_parse::ConfigWarning::auth_provider(
                "litellm",
                Some("command"),
                crate::agent::config_model_override_parse::ConfigWarningKind::InvalidValue,
                "missing or empty command".to_owned(),
            );
        let human = render_config_warnings(&[provider_warning]);
        assert!(
            human.contains("[auth_provider.\"litellm\"] command"),
            "{human}"
        );
        // A dotted provider name renders whole; the field splits off the right
        let dotted = crate::agent::config_model_override_parse::ConfigWarning::auth_provider(
            "corp.gateway",
            Some("token_ttl_secs"),
            crate::agent::config_model_override_parse::ConfigWarningKind::InvalidValue,
            "at or below the refresh margin".to_owned(),
        );
        let human = render_config_warnings(&[dotted]);
        assert!(
            human.contains("[auth_provider.\"corp.gateway\"] token_ttl_secs"),
            "{human}"
        );
        assert_eq!(render_config_warnings(&[]), "");

        let json = serde_json::to_value(&warnings).unwrap();
        let alias_warning = json
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w.get("field").and_then(|v| v.as_str()) == Some("send_compactions_remaining"))
            .expect("alias warning present in JSON");
        assert_eq!(
            alias_warning.get("target").and_then(|v| v.as_str()),
            Some("model")
        );
        assert_eq!(
            alias_warning.get("key").and_then(|v| v.as_str()),
            Some("grok-4.5")
        );
        assert_eq!(
            alias_warning.get("kind").and_then(|v| v.as_str()),
            Some("duplicate-alias")
        );
        assert!(
            alias_warning
                .get("reason")
                .and_then(|v| v.as_str())
                .is_some_and(|r| !r.is_empty())
        );
    }

    // ── skill source mapping (skill_entry_source) ─────────────────────────

    fn skill_fixture(name: &str, path: &str, scope: SkillScope) -> SkillInfo {
        SkillInfo {
            name: name.to_string(),
            description: format!("desc for {name}"),
            path: path.to_string(),
            scope,
            ..SkillInfo::default()
        }
    }

    #[test]
    fn skill_entry_source_maps_scopes() {
        let s = skill_fixture("a", "/repo/.grok/skills/a/SKILL.md", SkillScope::Local);
        assert!(matches!(
            skill_entry_source(&s),
            ConfigSource::Project { .. }
        ));

        let s = skill_fixture("b", "/repo/.grok/skills/b/SKILL.md", SkillScope::Repo);
        assert!(matches!(
            skill_entry_source(&s),
            ConfigSource::Project { .. }
        ));

        let s = skill_fixture("c", "/home/u/.grok/skills/c/SKILL.md", SkillScope::User);
        assert!(matches!(skill_entry_source(&s), ConfigSource::User { .. }));

        let s = skill_fixture(
            "d",
            "/home/u/.grok/server-skills/d/SKILL.md",
            SkillScope::Server,
        );
        assert!(matches!(
            skill_entry_source(&s),
            ConfigSource::Server { .. }
        ));

        let s = skill_fixture("e", "/home/u/.grok/bundled/e/SKILL.md", SkillScope::Bundled);
        assert!(matches!(
            skill_entry_source(&s),
            ConfigSource::Bundled { .. }
        ));
    }

    fn blank_skill_entry(skill: &SkillInfo) -> SkillEntry {
        SkillEntry {
            name: skill.label().to_string(),
            description: skill.description.clone(),
            source: ConfigSource::User {
                path: PathBuf::from(&skill.path),
            },
            user_invocable: skill.user_invocable,
            vendor: None,
            disabled: !skill.enabled,
            compatibility_status: None,
            collides_with: None,
            invocable_as: None,
        }
    }

    fn collision_entry(skill: &SkillInfo, all: &[SkillInfo]) -> SkillEntry {
        let mut entry = blank_skill_entry(skill);
        let (collides_with, invocable_as) = slash_collision(skill, &slash_name_counts(all));
        entry.collides_with = collides_with;
        entry.invocable_as = invocable_as;
        entry
    }

    #[test]
    fn apply_slash_collision_flags_reserved_names_and_duplicates() {
        let mut login = skill_fixture(
            "login",
            "/plugins/acme/skills/login/SKILL.md",
            SkillScope::Plugin,
        );
        login.plugin_name = Some("acme".into());
        let deploy = skill_fixture("deploy", "/tmp/deploy/SKILL.md", SkillScope::Local);
        // Gated builtins like /goal stay untagged: inspect must not invent /local:goal while the live catalog may still advertise /goal
        let goal = skill_fixture("goal", "/tmp/goal/SKILL.md", SkillScope::Local);
        let commit_local = skill_fixture("commit", "/tmp/l/commit/SKILL.md", SkillScope::Local);
        let commit_user = skill_fixture("commit", "/tmp/u/commit/SKILL.md", SkillScope::User);
        let all = [login, deploy, goal, commit_local, commit_user];
        let [login, deploy, goal, commit_local, commit_user] = &all;

        let entry = collision_entry(login, &all);
        assert_eq!(entry.collides_with.as_deref(), Some("login"));
        assert_eq!(entry.invocable_as.as_deref(), Some("acme:login"));

        for skill in [deploy, goal] {
            let entry = collision_entry(skill, &all);
            assert_eq!(entry.collides_with, None, "{}", skill.name);
            assert_eq!(entry.invocable_as, None, "{}", skill.name);
        }

        let entry = collision_entry(commit_local, &all);
        assert_eq!(entry.collides_with.as_deref(), Some("commit"));
        assert_eq!(entry.invocable_as.as_deref(), Some("local:commit"));
        let entry = collision_entry(commit_user, &all);
        assert_eq!(entry.invocable_as.as_deref(), Some("user:commit"));
    }

    #[test]
    fn apply_slash_collision_folds_reserved_name_case() {
        let skill = skill_fixture("Login", "/tmp/Login/SKILL.md", SkillScope::Local);
        let entry = collision_entry(&skill, std::slice::from_ref(&skill));
        assert_eq!(entry.collides_with.as_deref(), Some("Login"));
        assert_eq!(entry.invocable_as.as_deref(), Some("local:login"));
    }

    #[test]
    fn apply_slash_collision_withholds_contested_qualified_names() {
        let a = skill_fixture("commit", "/tmp/a/commit/SKILL.md", SkillScope::Local);
        let b = skill_fixture("commit", "/tmp/b/commit/SKILL.md", SkillScope::Local);
        let all = [a, b];
        let Some(first) = all.first() else {
            panic!("expected a skill fixture");
        };
        let entry = collision_entry(first, &all);
        assert_eq!(entry.collides_with.as_deref(), Some("commit"));
        assert_eq!(entry.invocable_as, None);
    }

    /// A `config_source` stamped at discovery (plugins, `[skills].paths`) wins over the scope fallback.
    #[test]
    fn skill_entry_source_prefers_stamped_config_source() {
        let mut s = skill_fixture("cfg", "/team/skills/cfg/SKILL.md", SkillScope::User);
        s.config_source = Some(ConfigSource::ConfigToml {
            path: PathBuf::from("/team/skills/cfg/SKILL.md"),
        });
        assert!(matches!(
            skill_entry_source(&s),
            ConfigSource::ConfigToml { .. }
        ));
    }

    /// `list_skills` must honor the `[skills]` table like the runtime does.
    /// `paths` skills appear (with a `configToml` source), `ignore`d skills are hidden, and `disabled` skills stay listed but flagged.
    #[tokio::test]
    async fn list_skills_honors_skills_config() {
        let write = |dir: &Path, name: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: test skill {name}\n---\n\nBody.\n"),
            )
            .unwrap();
        };
        // Test-unique names: discovery also reads this machine's real ~/.grok dirs.
        let extra = tempfile::tempdir().unwrap();
        write(&extra.path().join("inspect-cfg-extra"), "inspect-cfg-extra");
        write(
            &extra.path().join("inspect-cfg-ignored"),
            "inspect-cfg-ignored",
        );

        let cwd = tempfile::tempdir().unwrap();
        let config = SkillsConfig {
            paths: vec![extra.path().to_string_lossy().into_owned()],
            ignore: vec![
                extra
                    .path()
                    .join("inspect-cfg-ignored")
                    .to_string_lossy()
                    .into_owned(),
            ],
            disabled: vec!["inspect-cfg-extra".to_string()],
            ..Default::default()
        };
        let registry = distill_agent::plugins::PluginRegistry::from_discovered(vec![], &[], &[]);

        let entries = list_skills(
            cwd.path(),
            &registry,
            &config,
            /*project_trusted*/ true,
        )
        .await;

        let extra_entry = entries
            .iter()
            .find(|e| e.name == "inspect-cfg-extra")
            .expect("[skills].paths skill should be listed");
        assert!(
            matches!(extra_entry.source, ConfigSource::ConfigToml { .. }),
            "unexpected source: {:?}",
            extra_entry.source
        );
        assert!(
            extra_entry.disabled,
            "[skills].disabled must flag the entry"
        );
        assert!(
            !entries.iter().any(|e| e.name == "inspect-cfg-ignored"),
            "[skills].ignore must hide the skill"
        );
    }

    /// A vendor on a listed dir's entry would let `build_report` mark it disabled when that compat cell is off.
    #[tokio::test]
    async fn list_instructions_reports_configured_dirs_as_global_rules_without_vendor() {
        // Discovery also reads this machine's real home dirs, so assert only on the scratch entries.
        let home = tempfile::tempdir().unwrap();
        let rules = home.path().join(".claude").join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        let rule = rules.join("imported.md");
        std::fs::write(&rule, "imported rule").unwrap();
        let cwd = tempfile::tempdir().unwrap();

        let without = list_instructions(
            cwd.path(),
            &PathsConfig::default(),
            /*project_trusted*/ true,
        )
        .await;
        assert!(
            !without.iter().any(|e| Path::new(&e.path) == rule),
            "an unlisted dir must not be scanned"
        );

        let with = list_instructions(
            cwd.path(),
            &paths_config(&rules),
            /*project_trusted*/ true,
        )
        .await;
        let entry = with
            .iter()
            .find(|e| Path::new(&e.path) == rule)
            .expect("[paths].extra_rule_dirs rule should be listed");
        assert_eq!("rules", entry.file_type);
        assert!(matches!(entry.scope, Scope::Global), "{:?}", entry.scope);
        assert_eq!(None, entry.vendor);

        let claude_rules_off = ExternalCompatReport {
            remote_settings_loaded: false,
            cells: vec![ExternalCompatEntry {
                vendor: "claude".to_owned(),
                surface: "rules".to_owned(),
                enabled: false,
                source: CompatSource::Config,
            }],
        };
        assert_eq!(
            None,
            instruction_compat_status(&entry.vendor, &entry.file_type, &claude_rules_off)
        );
    }

    struct FailAfter {
        remaining: usize,
        kind: std::io::ErrorKind,
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::new(self.kind, "closed"));
            }
            let n = buf.len().min(self.remaining);
            self.remaining -= n;
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn empty_report() -> InspectReport {
        InspectReport {
            grok_version: "test".into(),
            channel: "test".into(),
            cwd: "/tmp".into(),
            project_root: None,
            project_trusted: true,
            project_instructions: vec![],
            permissions: PermissionsReport {
                sources: vec![],
                loaded: 0,
                skipped: vec![],
                mcp_server_allowlist: vec![],
                mcp_lockdown_sources: vec![],
                mcp_managed_servers_only: ManagedOnlyScope::Off,
                marketplace_allowlist: vec![],
                marketplace_lockdown_sources: vec![],
                managed_marketplaces: vec![],
                managed_settings_path: None,
                managed_settings_exists: false,
                managed_settings_active: false,
                enforced: vec![],
                claude_bypass_lock_advisory: false,
            },
            login_policy: LoginPolicyReport {
                disable_api_key_auth: None,
                force_login_team_uuid: None,
                api_key_auth_disabled: false,
            },
            hooks: vec![],
            skills: vec![],
            agents: vec![],
            plugins: vec![],
            marketplaces: vec![],
            mcp_servers: vec![],
            lsp_servers: vec![],
            config_sources: ConfigSources { layers: vec![] },
            external_compat: ExternalCompatReport {
                remote_settings_loaded: false,
                cells: vec![],
            },
            config_warnings: vec![],
            mcp_config_problems: vec![],
        }
    }

    #[test]
    fn write_inspect_treats_broken_pipe_as_success() {
        let report = empty_report();
        for json in [false, true] {
            let mut out = FailAfter {
                remaining: 8,
                kind: std::io::ErrorKind::BrokenPipe,
            };
            write_inspect(&report, json, &mut out).expect("broken pipe is a clean stop");
        }
    }

    /// The managed-policy lines must go through the injected writer, never `println!`.
    #[test]
    fn print_human_policy_lines_use_the_injected_writer() {
        let mut report = empty_report();
        report.permissions.mcp_managed_servers_only = ManagedOnlyScope::Enforced;
        report.permissions.enforced = vec![
            EnforcedPolicy {
                setting: EnforcedSetting::ProjectMcpServers,
                enabled: false,
                source: "/etc/grok/managed_config.toml".into(),
            },
            EnforcedPolicy {
                setting: EnforcedSetting::PluginAutoUpdate,
                enabled: false,
                source: "/etc/grok/managed_config.toml".into(),
            },
        ];
        report.permissions.managed_marketplaces =
            vec!["approved-plugins (https://github.com/corp/approved.git@stable)".into()];

        let mut out = Vec::new();
        print_human(&report, &mut out).expect("buffer write succeeds");
        let text = String::from_utf8(out).unwrap();
        for needle in [
            "MCP managed servers only: enforced",
            "Project MCP servers disabled (/etc/grok/managed_config.toml)",
            "Plugin auto-update disabled (/etc/grok/managed_config.toml)",
            "Managed marketplaces (1 pinned)",
            "approved-plugins (https://github.com/corp/approved.git@stable)",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }

        // An advisory-only lockdown must not read as "enforced".
        report.permissions.mcp_managed_servers_only = ManagedOnlyScope::Advisory;
        let mut out = Vec::new();
        print_human(&report, &mut out).expect("buffer write succeeds");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("MCP managed servers only: advisory"),
            "{text}"
        );
        assert!(
            !text.contains("MCP managed servers only: enforced"),
            "{text}"
        );
    }

    fn native_lockdown(source: &str) -> LockdownSource {
        LockdownSource {
            source: source.to_string(),
            advisory: false,
        }
    }

    /// A zero-entry lockdown blocks everything while contributing no entries;
    /// the human report must name the enforcing file, not render unrestricted.
    #[test]
    fn print_human_surfaces_zero_entry_lockdowns() {
        let mut report = empty_report();
        report.permissions.mcp_lockdown_sources =
            vec![native_lockdown("/etc/grok/managed_config.toml")];
        report.permissions.marketplace_lockdown_sources =
            vec![native_lockdown("/etc/grok/managed_config.toml")];

        let mut out = Vec::new();
        print_human(&report, &mut out).expect("buffer write succeeds");
        let text = String::from_utf8(out).unwrap();
        for needle in [
            "MCP servers locked down",
            "Marketplaces locked down",
            "/etc/grok/managed_config.toml",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(
            !text.contains("advisory"),
            "native sources carry no tag:\n{text}"
        );
    }

    /// JSON consumers see each lockdown source with its authority under camelCase names.
    #[test]
    fn json_output_carries_lockdown_fields() {
        let mut report = empty_report();
        report.permissions.mcp_lockdown_sources =
            vec![native_lockdown("/etc/grok/managed_config.toml")];
        report.permissions.marketplace_lockdown_sources = vec![LockdownSource {
            source: "managed-settings.json".into(),
            advisory: true,
        }];
        let json = serde_json::to_value(&report).unwrap();
        let Some(json) = json.get("permissions") else {
            panic!("expected permissions in report JSON");
        };
        assert_eq!(
            json.get("mcpLockdownSources"),
            Some(
                &serde_json::json!([{ "source": "/etc/grok/managed_config.toml", "advisory": false }])
            )
        );
        assert_eq!(
            json.get("marketplaceLockdownSources"),
            Some(&serde_json::json!([{ "source": "managed-settings.json", "advisory": true }]))
        );
    }

    /// The report's lockdown lists come from the loaded policy: a zero-entry
    /// lockdown source surfaces, a populated source does not.
    #[test]
    fn lockdown_sources_derive_from_policy() {
        use distill_workspace::permission::resolution::{
            AllowedMcpServer, ManagedSettings, MarketplaceAllowlist, McpServerAllowlist,
            McpServerPolicy, PolicySourceAuthority,
        };
        let mut ms = ManagedSettings::default();
        ms.mcp_allowlist = McpServerPolicy::single(
            McpServerAllowlist::new(
                vec![],
                vec![],
                Some(std::path::PathBuf::from("/etc/grok/managed_config.toml")),
            )
            .with_lockdown(),
        );
        ms.mcp_allowlist.sources.push(McpServerAllowlist::new(
            vec![AllowedMcpServer::Http {
                url_pattern: "https://ok.example.com/*".into(),
            }],
            vec![],
            Some(std::path::PathBuf::from("/etc/grok/requirements.toml")),
        ));
        ms.marketplace_allowlist.sources.push(MarketplaceAllowlist {
            allowed_urls: vec![],
            source_path: Some(std::path::PathBuf::from("/etc/grok/managed_config.toml")),
            authority: PolicySourceAuthority::Native,
        });

        let (mcp, marketplace) = policy_lockdown_sources(&ms);
        for sources in [mcp, marketplace] {
            let [src] = sources.as_slice() else {
                panic!("expected one lockdown source: {sources:?}");
            };
            assert_eq!(src.source, "/etc/grok/managed_config.toml");
            assert!(!src.advisory);
        }
    }

    /// An advisory lockdown binds foreign-defined subjects only; the report must
    /// say so instead of announcing a lockdown grok-native servers are exempt from.
    #[test]
    fn advisory_lockdown_sources_are_tagged() {
        use distill_workspace::permission::resolution::{
            ManagedSettings, MarketplaceAllowlist, McpServerAllowlist, McpServerPolicy,
            PolicySourceAuthority,
        };
        let vendor = || Some(std::path::PathBuf::from("managed-settings.json"));
        let mut ms = ManagedSettings::default();
        ms.mcp_allowlist = McpServerPolicy::single(
            McpServerAllowlist::new(vec![], vec![], vendor())
                .with_lockdown()
                .with_authority(PolicySourceAuthority::Advisory),
        );
        ms.marketplace_allowlist.sources.push(MarketplaceAllowlist {
            allowed_urls: vec![],
            source_path: vendor(),
            authority: PolicySourceAuthority::Advisory,
        });

        let (mcp, marketplace) = policy_lockdown_sources(&ms);
        let mut report = empty_report();
        report.permissions.mcp_lockdown_sources = mcp;
        report.permissions.marketplace_lockdown_sources = marketplace;
        let mut out = Vec::new();
        print_human(&report, &mut out).expect("buffer write succeeds");
        let text = String::from_utf8(out).unwrap();
        for needle in [
            "managed-settings.json (advisory; grok-native servers exempt)",
            "managed-settings.json (advisory; grok-native marketplaces exempt)",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn write_inspect_surfaces_other_io_errors() {
        let report = empty_report();
        let mut out = FailAfter {
            remaining: 0,
            kind: std::io::ErrorKind::PermissionDenied,
        };
        write_inspect(&report, false, &mut out)
            .expect_err("non-broken-pipe IO errors must surface");
    }
}
