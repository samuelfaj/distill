// Modified for Distill by Samuel Fajreldines, 2026.
//! Merges native `.grok/config.toml`, managed/enterprise settings, and `.claude` settings into the effective `PermissionConfig`.
//! Also holds the MCP-server and marketplace allowlists and the always-approve policy pin.

use crate::permission::claude_settings::*;
use crate::permission::rules::*;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use tracing::{debug, info, warn};

use crate::permission::types::{
    PatternMode, PermissionConfig, PermissionRule, PromptPolicy, RuleAction, ToolFilter,
};

/// Whether user/project/local files should apply their own `defaultMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserDefaultModeLoad {
    /// Apply most-specific user/project/local `defaultMode`.
    Apply,
    /// Managed-settings already owns the mode; load allow/deny/ask only.
    SkipManagedOwns,
}

/// Synthetic rules and skip records for `acceptEdits` / `bypassPermissions`.
///
/// Shared by managed and user-tier application so pin handling cannot drift.
fn synthetic_rules_for_default_mode(
    mode: DefaultPermissionMode,
    policy_block: Option<&str>,
) -> (
    Vec<PermissionRule>,
    Vec<SkippedPermission>,
    bool, /* bypass_blocked */
) {
    let effects = mode.effects();
    let mut rules = Vec::new();
    let mut skipped = Vec::new();
    let mut bypass_blocked = false;

    if effects.bypass_permissions {
        if let Some(reason) = policy_block {
            warn!("defaultMode=bypassPermissions ignored: disabled by managed policy");
            bypass_blocked = true;
            skipped.push(SkippedPermission {
                rule: "defaultMode=bypassPermissions".to_string(),
                reason: reason.to_string(),
            });
        } else {
            debug!("defaultMode=bypassPermissions: appending catch-all Allow Any rule");
            rules.push(PermissionRule {
                action: RuleAction::Allow,
                tool: ToolFilter::Any,
                pattern: None,
                pattern_mode: PatternMode::Glob,
            });
        }
    } else if effects.accept_edits {
        debug!("defaultMode=acceptEdits: appending synthetic Allow Edit rule");
        rules.push(PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Edit,
            pattern: None,
            pattern_mode: PatternMode::Glob,
        });
    }

    (rules, skipped, bypass_blocked)
}

/// Parse a defaultMode string; an unknown value fails safe to [`DefaultPermissionMode::Default`] with a warn and a skip record for `grok inspect`.
fn parse_default_mode_claiming_scope(
    raw: &str,
    path: &Path,
    skipped: &mut Vec<SkippedPermission>,
) -> DefaultPermissionMode {
    match DefaultPermissionMode::from_str(raw) {
        Ok(mode) => mode,
        Err(invalid) => {
            warn!(
                path = %path.display(),
                default_mode = %invalid,
                "settings: unrecognized defaultMode value; treating as default (prompt)"
            );
            skipped.push(SkippedPermission {
                rule: format!("defaultMode={invalid}"),
                reason: "unrecognized value; treated as default".to_string(),
            });
            DefaultPermissionMode::Default
        }
    }
}

/// Parse `[permission]` from TOML.
/// Tries compact (`deny = ["Read(...)"]`) first, falls back to verbose (`[[permission.rules]]`).
fn parse_toml_permission_section(
    permission_value: &toml::Value,
) -> Result<Vec<PermissionRule>, String> {
    let mut rules = Vec::new();
    let mut found_compact = false;

    for (action, key) in [
        (RuleAction::Deny, "deny"),
        (RuleAction::Allow, "allow"),
        (RuleAction::Ask, "ask"),
    ] {
        if let Some(value) = permission_value.get(key) {
            let Some(arr) = value.as_array() else {
                // Don't drop a security rule list silently.
                warn!(
                    "permission.{key}: expected an array of rule strings, got {} -- ignored",
                    toml_type_name(value)
                );
                continue;
            };
            found_compact = true;
            for (i, item) in arr.iter().enumerate() {
                if let Some(s) = item.as_str() {
                    match parse_permission_rule(s, action) {
                        Ok(rule) => rules.push(rule),
                        Err(e) => warn!("permission.{key}[{i}]: \"{s}\" -- {e}"),
                    }
                } else {
                    warn!(
                        "permission.{key}[{i}]: expected string, got {}",
                        toml_type_name(item)
                    );
                }
            }
        }
    }

    if found_compact {
        return Ok(rules);
    }

    permission_value
        .clone()
        .try_into::<PermissionConfig>()
        .map(|config| config.rules)
        .map_err(|e| e.to_string())
}

fn toml_type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

use crate::permission::types::{RequirementSource, Sourced};

/// Try to extract `[permission]` rules from a TOML config value.
fn extract_toml_permissions(
    config: &toml::Value,
    make_source: impl Fn() -> RequirementSource,
) -> Vec<Sourced<PermissionRule>> {
    let Some(permission_value) = config.get("permission") else {
        return Vec::new();
    };
    match parse_toml_permission_section(permission_value) {
        Ok(rules) => {
            let source = make_source();
            if !rules.is_empty() {
                info!(count = rules.len(), %source, "Loaded permission rules");
            }
            rules
                .into_iter()
                .map(|rule| Sourced {
                    value: rule,
                    source: source.clone(),
                })
                .collect()
        }
        Err(e) => {
            let source = make_source();
            warn!(error = %e, %source, "Failed to parse [permission]");
            Vec::new()
        }
    }
}

/// Load `[permission]` rules from requirements.toml layers. Trust keys on the
/// `is_system` flag (set at load, never from `path`): system → `SystemRequirements`,
/// user `~/.grok` → `Requirements`, so [`is_admin_source`] trusts only the root tier.
fn load_requirements_permissions() -> Vec<Sourced<PermissionRule>> {
    distill_config::requirements_layers()
        .into_iter()
        .flat_map(|layer| {
            let source = if layer.is_system {
                RequirementSource::SystemRequirements {
                    path: PathBuf::from(layer.source.label().as_ref()),
                }
            } else {
                RequirementSource::Requirements {
                    path: PathBuf::from(layer.source.label().as_ref()),
                }
            };
            extract_toml_permissions(&layer.value, || source.clone())
        })
        .collect()
}

/// Load `[permission]` rules from `~/.grok/config.toml` (lowest) then each `.grok/config.toml` from repo root to `cwd`.
/// The walk matches [`crate::project_config::find_project_configs`] so detector and loader agree. Empty if no `[permission]` section.
fn load_config_toml_permissions(cwd: &Path, project_trusted: bool) -> Vec<Sourced<PermissionRule>> {
    let mut rules = Vec::new();

    // Global `~/.grok/config.toml` first (lowest priority within this layer).
    // Gated on user_distill_home() so a project's .grok/config.toml is never read as global permissions when neither GROK_HOME nor a home dir resolves
    if let Some(global_path) = distill_config::user_distill_home().map(|g| g.join("config.toml"))
        && global_path.is_file()
    {
        match distill_config::load_config_file(&global_path) {
            Ok(value) => rules.extend(extract_toml_permissions(&value, || {
                RequirementSource::Config {
                    path: global_path.clone(),
                }
            })),
            Err(e) => {
                warn!(path = %global_path.display(), error = %e, "Failed to load global config.toml")
            }
        }
    }

    // Project-scoped configs walking from git root down to cwd, gated on trust.
    // An untrusted clone must not contribute allow/deny/ask rules via `.grok/config.toml` (same gate as project `.claude/settings.json`)
    if project_trusted {
        for path in crate::project_config::find_project_configs(cwd) {
            match distill_config::load_config_file(&path) {
                Ok(value) => rules.extend(extract_toml_permissions(&value, || {
                    RequirementSource::Config { path: path.clone() }
                })),
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Failed to load project config.toml")
                }
            }
        }
    }

    rules
}

fn managed_config_permissions(
    layers: &[distill_config::ManagedConfigLayer],
) -> Vec<Sourced<PermissionRule>> {
    layers
        .iter()
        .flat_map(|layer| {
            extract_toml_permissions(&layer.value, || RequirementSource::ManagedConfig {
                path: layer.path.clone(),
            })
        })
        .collect()
}

// ═════════════════════════════════════════════════════════════════════════════
// Fallback Resolver
// ═════════════════════════════════════════════════════════════════════════════

/// Resolve permission config, merging native Grok and Claude sources. Evaluation is deny > ask > allow; merge order is provenance only.
/// Claude `acceptEdits` appends a synthetic `Allow Edit`. `project_trusted` gates project-tier rules; global/user/admin always load. Hub/cloud defaults trusted.
pub async fn resolve_permission_config_with_fallback(
    cwd: &Path,
    project_trusted: bool,
) -> Option<PermissionConfig> {
    resolve_permissions_with_provenance(cwd, project_trusted)
        .await
        .resolved
        .map(|r| r.config)
}

/// [`resolve_permission_config_with_fallback`] with the always-approve pin supplied by the caller.
/// A session spawn reads the pin once and feeds that read here and to its other pin consumers (CLI catch-all drop, mode hint, manager clamp).
/// A requirements.toml edit landing between independent reads could otherwise leave those consumers disagreeing about the lock.
pub async fn resolve_permission_config_with_fallback_pinned(
    cwd: &Path,
    project_trusted: bool,
    yolo_lock: Option<&YoloPolicyLock>,
) -> Option<PermissionConfig> {
    resolve_permissions_with_provenance_inner(
        cwd,
        ResolveInputs::live(project_trusted, yolo_lock.cloned()),
    )
    .await
    .map(|r| r.config)
}

/// Client `startupHints.permissionMode` pre-declares an allow for headless agent-initiated turns after the client hangs up.
/// Any session-creating client may stamp it; it grants nothing a connected allow-all client could not, and never weakens deny or pre-prompt rejection.
/// The always-approve pin kills it fleet-wide, and an explicit `defaultMode` outranks it.
pub const PERMISSION_MODE_ALWAYS_ALLOW: &str = "alwaysAllow";

/// Apply `startupHints.permissionMode`. Only [`PERMISSION_MODE_ALWAYS_ALLOW`] is honored, and only if bypass is not pinned off.
/// Requires no explicit `permissions.defaultMode` ([`PermissionConfig::default_mode_configured`], not `prompt_policy`): modes that project to `Ask` must not be upgraded.
pub fn apply_permission_mode_hint(
    config: &mut Option<PermissionConfig>,
    requested: Option<&str>,
    policy_block: Option<&'static str>,
) -> bool {
    let Some(mode) = requested else { return false };
    if mode != PERMISSION_MODE_ALWAYS_ALLOW {
        warn!(mode, "unrecognized startupHints.permissionMode ignored");
        return false;
    }
    if let Some(reason) = policy_block {
        warn!(
            reason,
            "startupHints.permissionMode=alwaysAllow ignored: always-approve disabled by managed policy"
        );
        return false;
    }
    let config = config.get_or_insert_with(|| PermissionConfig::new(Vec::new()));
    if config.default_mode_configured {
        warn!(
            prompt_policy = ?config.prompt_policy,
            "startupHints.permissionMode=alwaysAllow ignored: permissions.defaultMode is explicitly configured"
        );
        return false;
    }
    config.prompt_policy = PromptPolicy::Allow;
    info!("session permission prompts resolve as allow (startupHints.permissionMode)");
    true
}

/// `Deny` patterns that forbid reading a path (`Read`, `Grep`, or `Any`). Write-only denies and non-deny actions are excluded.
/// Public so Grep excludes come from the manager's effective config; re-resolving would miss CLI read denies.
pub fn deny_read_globs_from_config(config: &PermissionConfig) -> Vec<String> {
    config
        .rules
        .iter()
        .filter(|r| {
            r.action == RuleAction::Deny
                && matches!(
                    r.tool,
                    ToolFilter::Read | ToolFilter::Grep | ToolFilter::Any
                )
        })
        .filter_map(|r| r.pattern.clone())
        .collect()
}

/// Outcome of [`resolve_permissions_with_provenance`]: the rule resolution
/// plus the single always-approve pin read it used. The pin is carried even
/// when nothing resolves, so callers never re-read it for their own reporting.
pub struct ProvenanceResolution {
    /// The pin read this resolution used ([`yolo_policy_lock`]).
    pub yolo_lock: Option<YoloPolicyLock>,
    /// `None` when no permission sources are configured.
    pub resolved: Option<ResolvedPermissions>,
}

/// Result of permission resolution with provenance metadata.
pub struct ResolvedPermissions {
    pub config: PermissionConfig,
    /// `sources[i]` is where `config.rules[i]` came from.
    pub sources: Vec<RequirementSource>,
    /// Rules from `.claude/settings.json` that couldn't be parsed (empty for TOML).
    pub skipped: Vec<SkippedPermission>,
    /// The always-approve pin this resolution applied ([`yolo_policy_lock`]).
    pub yolo_lock: Option<YoloPolicyLock>,
}

/// A permission rule that was recognized but not loaded.
pub struct SkippedPermission {
    pub rule: String,
    pub reason: String,
}

fn tag_with_source(
    target: &mut Vec<Sourced<PermissionRule>>,
    rules: Vec<PermissionRule>,
    source: RequirementSource,
) {
    target.extend(rules.into_iter().map(|rule| Sourced {
        value: rule,
        source: source.clone(),
    }));
}

/// Whether an Allow rule is a blanket `--yolo` substitute the pin must drop.
/// That means a catch-all on `Any` or on a freeform authority dimension (Bash/MCP/WebFetch/AgentMessage); Read/Edit/Grep catch-alls survive.
pub fn is_catchall_allow(rule: &PermissionRule) -> bool {
    if rule.action != RuleAction::Allow {
        return false;
    }
    if matches!(
        rule.tool,
        ToolFilter::Read | ToolFilter::Edit | ToolFilter::Grep
    ) {
        return false;
    }
    crate::permission::policy::rule_is_catchall(rule)
}

/// Root-owned tiers whose catch-all allows survive the pin (managed-settings, system requirements).
/// Keyed on provenance, never a spoofable `path`.
fn is_admin_source(source: &RequirementSource) -> bool {
    matches!(
        source,
        RequirementSource::SystemRequirements { .. } | RequirementSource::ManagedSettings { .. }
    )
}

/// Under the pin, drop untrusted catch-all Allow rules (they substitute for the blocked `--yolo`); keep admin-tier ones.
/// Records each drop for `grok inspect`.
fn drop_untrusted_catchall_allows(
    rules: Vec<Sourced<PermissionRule>>,
    policy_block: Option<&'static str>,
    skipped: &mut Vec<SkippedPermission>,
) -> Vec<Sourced<PermissionRule>> {
    let Some(reason) = policy_block else {
        return rules;
    };
    rules
        .into_iter()
        .filter(|sourced| {
            if is_catchall_allow(&sourced.value) && !is_admin_source(&sourced.source) {
                warn!(
                    source = %sourced.source,
                    "catch-all allow rule ignored: always-approve disabled by managed policy"
                );
                skipped.push(SkippedPermission {
                    rule: format!(
                        "allow {} (catch-all)",
                        sourced.value.pattern.as_deref().unwrap_or("*")
                    ),
                    reason: reason.to_string(),
                });
                false
            } else {
                true
            }
        })
        .collect()
}

/// Inputs to [`resolve_permissions_with_provenance_inner`].
/// Production uses [`ResolveInputs::live`]; tests construct the fields directly so they never read the host's real managed files.
struct ResolveInputs<'a> {
    yolo_lock: Option<YoloPolicyLock>,
    managed: &'a ManagedSettings,
    managed_config_rules: Vec<Sourced<PermissionRule>>,
    /// Folder-trust verdict for `cwd`.
    /// When false, project-tier `.claude/settings.json` / `.grok/config.toml` permission rules are dropped (global/user/admin tiers still load).
    project_trusted: bool,
}

impl ResolveInputs<'static> {
    /// Production inputs; the caller supplies the always-approve pin so one
    /// read serves the whole flow.
    fn live(project_trusted: bool, yolo_lock: Option<YoloPolicyLock>) -> Self {
        Self {
            yolo_lock,
            managed: managed_settings(),
            managed_config_rules: managed_config_permissions(
                &distill_config::managed_config_layers(),
            ),
            project_trusted,
        }
    }
}

/// Collect rules from every source with origin. Deny beats ask beats allow regardless of file; source order is display only. Read at session start.
/// Managed `defaultMode` outranks user/project/local. Always-approve is independent and outranks `dontAsk` unless `requirements.toml` pins bypass off.
/// `project_trusted` gates project-tier rules so an untrusted clone cannot disable prompts. The outcome always carries the pin used.
pub async fn resolve_permissions_with_provenance(
    cwd: &Path,
    project_trusted: bool,
) -> ProvenanceResolution {
    let yolo_lock = yolo_policy_lock();
    let resolved = resolve_permissions_with_provenance_inner(
        cwd,
        ResolveInputs::live(project_trusted, yolo_lock.clone()),
    )
    .await;
    ProvenanceResolution {
        yolo_lock,
        resolved,
    }
}

async fn resolve_permissions_with_provenance_inner(
    cwd: &Path,
    inputs: ResolveInputs<'_>,
) -> Option<ResolvedPermissions> {
    let ResolveInputs {
        yolo_lock,
        managed,
        managed_config_rules,
        project_trusted,
    } = inputs;
    let policy_block = yolo_lock.as_ref().map(|lock| lock.reason.message());
    let config_toml_rules = load_config_toml_permissions(cwd, project_trusted);

    // Managed defaultMode wins; skip user-tier defaultMode application so a project acceptEdits cannot loosen a managed dontAsk/auto/default
    let managed_mode = managed.default_mode;
    let user_mode_load = if managed_mode.is_some() {
        UserDefaultModeLoad::SkipManagedOwns
    } else {
        UserDefaultModeLoad::Apply
    };

    // Phase 2 cutoff: skip the .claude/ fallback once the user has imported.
    // Native config-derived permissions still apply.
    let skip_claude = is_claude_import_marked_with_log("resolve_permissions_with_provenance");
    let settings_json = if skip_claude {
        None
    } else {
        resolve_claude_settings_inner(cwd, project_trusted, policy_block, user_mode_load)
    };

    let mut all_rules: Vec<Sourced<PermissionRule>> = Vec::new();
    all_rules.extend(load_requirements_permissions());
    all_rules.extend(managed.permissions.clone());

    let mut skipped = Vec::new();
    let mut prompt_policy = PromptPolicy::default();
    let mut default_mode_configured = false;

    // Apply managed defaultMode synthetics and prompt policy (highest mode tier)
    if let Some(mode) = managed_mode {
        default_mode_configured = true;
        prompt_policy = mode.effects().prompt_policy;
        let managed_path = managed
            .features
            .source_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("managed-settings.json"));
        let source = RequirementSource::ManagedSettings { path: managed_path };
        let (syn_rules, syn_skipped, _) = synthetic_rules_for_default_mode(mode, policy_block);
        skipped.extend(syn_skipped);
        for rule in syn_rules {
            all_rules.push(Sourced {
                value: rule,
                source: source.clone(),
            });
        }
    }

    all_rules.extend(managed_config_rules);
    all_rules.extend(config_toml_rules);
    if let Some((config, skipped_rules, path)) = settings_json {
        skipped.extend(skipped_rules);
        // User-tier prompt_policy only when managed did not set defaultMode.
        if managed_mode.is_none() {
            prompt_policy = config.prompt_policy;
            default_mode_configured = config.default_mode_configured;
        }
        tag_with_source(
            &mut all_rules,
            config.rules,
            RequirementSource::Settings { path },
        );
    }

    // Must run while provenance is in scope (discarded by the unzip below)
    // CLI `--allow '*'` is filtered at its own merge site (acp_session)
    let all_rules = drop_untrusted_catchall_allows(all_rules, policy_block, &mut skipped);

    // Keep skip-only resolutions alive so the drop reaches `grok inspect`
    // A rule-less explicit defaultMode must survive: dropping it to `None` erases `default_mode_configured` and lets the alwaysAllow hint upgrade it
    if all_rules.is_empty()
        && prompt_policy == PromptPolicy::Ask
        && skipped.is_empty()
        && !default_mode_configured
    {
        return None;
    }

    let (rules, sources): (Vec<_>, Vec<_>) =
        all_rules.into_iter().map(|s| (s.value, s.source)).unzip();

    debug!(rules = rules.len(), "Resolved permission rules");

    Some(ResolvedPermissions {
        config: PermissionConfig {
            rules,
            prompt_policy,
            default_mode_configured,
        },
        sources,
        skipped,
        yolo_lock,
    })
}

/// Merge Claude allow/deny/ask across scopes so global grants survive a project file. `defaultMode` is most-specific-wins; an unrecognized value still claims the slot.
/// `bypassPermissions` is catch-all allow unless the yolo pin skips it; `acceptEdits`/`dontAsk`/`auto` map to synthetic allow, deny, and classifier. Synthetics are fallbacks.
/// Untrusted folders load only global `~/.claude`; project rules and `defaultMode` are dropped.
fn resolve_claude_settings_inner(
    cwd: &Path,
    project_trusted: bool,
    policy_block: Option<&'static str>,
    user_mode_load: UserDefaultModeLoad,
) -> Option<(PermissionConfig, Vec<SkippedPermission>, PathBuf)> {
    let mut all_rules = Vec::new();
    let mut all_skipped = Vec::new();
    let mut primary_source_path: Option<PathBuf> = None;
    // Track defaultMode from the most specific file (paths are most-specific-first).
    // Also track its source path so synthetic rules have provenance even when no explicit permissions block exists
    let mut default_mode_source: Option<PathBuf> = None;
    let mut applied_mode: Option<DefaultPermissionMode> = None;
    let mut prompt_policy = PromptPolicy::default();
    let mut files_with_rules: u32 = 0;

    // Same path set as env injection ([`claude_settings_paths_for_trust`]).
    for path in claude_settings_paths_for_trust(cwd, project_trusted) {
        let Some(settings) = load_claude_settings(&path) else {
            continue;
        };

        if let Some(dirs) = &settings.additional_directories {
            info!(
                path = %path.display(),
                count = dirs.len(),
                "Claude settings: additionalDirectories parsed but not supported"
            );
        }

        // defaultMode: most-specific file that *sets* the key wins, including typos (treated as default)
        // Skipped when managed-settings owns mode
        if user_mode_load == UserDefaultModeLoad::Apply
            && default_mode_source.is_none()
            && let Some(raw) = &settings.default_mode
        {
            default_mode_source = Some(path.clone());
            let mode = parse_default_mode_claiming_scope(raw, &path, &mut all_skipped);
            applied_mode = Some(mode);
            prompt_policy = mode.effects().prompt_policy;
        }

        if let Some(perms) = settings.permissions {
            let (cfg, warnings) = perms.into_permission_config();
            for w in &warnings {
                warn!(path = %path.display(), "{}", w);
            }
            // Rules *or* skip-only parse failures still own provenance for `grok inspect`
            // All-invalid allow/deny/ask must not leave primary_source_path unset and panic below
            if (!cfg.rules.is_empty() || !warnings.is_empty()) && primary_source_path.is_none() {
                primary_source_path = Some(path.clone());
            }
            if !cfg.rules.is_empty() {
                files_with_rules += 1;
                debug!(
                    path = %path.display(),
                    rules = cfg.rules.len(),
                    "Claude settings: loaded permission rules"
                );
            }
            all_rules.extend(cfg.rules);
            all_skipped.extend(warnings.into_iter().map(|w| {
                let (rule, reason) = w
                    .split_once(" \u{2014} ")
                    .or_else(|| w.split_once(" -- "))
                    .map_or((w.as_str(), ""), |(r, d)| (r, d));
                SkippedPermission {
                    rule: rule.to_string(),
                    reason: reason.to_string(),
                }
            }));
        }
    }

    let mut bypass_blocked = false;
    if let Some(mode) = applied_mode {
        let (syn_rules, syn_skipped, blocked) =
            synthetic_rules_for_default_mode(mode, policy_block);
        bypass_blocked = blocked;
        all_skipped.extend(syn_skipped);
        all_rules.extend(syn_rules);
    }

    // A blocked bypass, a claimed defaultMode (incl. a typo treated as default), or skip records still resolve (possibly zero rules).
    // Provenance then reaches `grok inspect` via the outer resolver
    if all_rules.is_empty()
        && prompt_policy == PromptPolicy::Ask
        && !bypass_blocked
        && default_mode_source.is_none()
        && all_skipped.is_empty()
    {
        return None;
    }

    if files_with_rules > 1 {
        info!(
            files = files_with_rules,
            total_rules = all_rules.len(),
            "Claude settings: merged permission rules from multiple files"
        );
    }

    // Prefer the first file with explicit permission rules or skip-only parse failures; fall back to the file that provided defaultMode
    // Never panic: a skip-only / mode-only resolution must always surface.
    let source_path = primary_source_path
        .or(default_mode_source)
        .unwrap_or_else(|| {
            warn!(
                cwd = %cwd.display(),
                skipped = all_skipped.len(),
                "Claude settings resolution has no settings file provenance; using cwd"
            );
            cwd.to_path_buf()
        });

    Some((
        PermissionConfig {
            rules: all_rules,
            prompt_policy,
            default_mode_configured: applied_mode.is_some(),
        },
        all_skipped,
        source_path,
    ))
}

// ═════════════════════════════════════════════════════════════════════════════
// managed-settings.json
// ═════════════════════════════════════════════════════════════════════════════

// The managed MCP/plugin/marketplace policy engine lives in
// [`super::managed_policy`]; re-exported here so existing
// `permission::resolution::` paths keep working.
pub use super::managed_policy::{
    AllowedMcpServer, MANAGED_POLICY_CONFIG_KEYS, ManagedMarketplace, ManagedMarketplaceKind,
    ManagedSettings, ManagedSettingsFeatures, MarketplaceAllowlist, MarketplacePolicy,
    McpBlockReason, McpServerAllowlist, McpServerPolicy, McpSubject, McpVerdict,
    PolicyLayerOwnership, PolicyPin, PolicySourceAuthority, PolicySubjectOrigin, managed_settings,
    normalize_git_url,
};

/// The non-policy managed-settings surface: features, permission rules, and
/// `permissions.defaultMode`.
pub(super) fn parse_managed_settings_base(
    json: &serde_json::Value,
    path: &Path,
) -> ManagedSettings {
    let env = json.get("env");
    let features = ManagedSettingsFeatures {
        disable_telemetry: json_env_flag(env, "DISABLE_TELEMETRY"),
        disable_feedback: json_env_flag(env, "DISABLE_FEEDBACK_COMMAND"),
        disable_yolo: parse_disable_bypass_permissions(json),
        source_path: Some(path.to_path_buf()),
    };

    let permissions = parse_managed_settings_permissions(json, path);
    let mut skipped = Vec::new();
    let default_mode = extract_default_mode(json, path).map(|raw| {
        let mode = parse_default_mode_claiming_scope(&raw, path, &mut skipped);
        info!(
            path = %path.display(),
            default_mode = %raw,
            "Loaded permissions.defaultMode from managed-settings.json"
        );
        for s in &skipped {
            warn!(path = %path.display(), rule = %s.rule, reason = %s.reason, "managed defaultMode");
        }
        mode
    });

    ManagedSettings {
        features,
        permissions,
        default_mode,
        ..ManagedSettings::default()
    }
}

fn parse_managed_settings_permissions(
    json: &serde_json::Value,
    path: &Path,
) -> Vec<Sourced<PermissionRule>> {
    let Some(perms_value) = json.get("permissions") else {
        return Vec::new();
    };
    let permissions: ParsedPermissions = match serde_json::from_value(perms_value.clone()) {
        Ok(p) => p,
        Err(e) => {
            // One wrong-typed field discards the whole block; without a log the
            // file still looks active (defaultMode parses separately).
            warn!(
                path = %path.display(),
                error = %e,
                "managed-settings `permissions` could not be parsed; ALL managed permission rules from this file are ignored"
            );
            return Vec::new();
        }
    };
    let (config, warnings) = permissions.into_permission_config();
    for w in &warnings {
        warn!(path = %path.display(), "{}", w);
    }
    if !config.rules.is_empty() {
        info!(
            path = %path.display(),
            count = config.rules.len(),
            "Loaded permission rules from managed-settings.json"
        );
    }
    let source = RequirementSource::ManagedSettings {
        path: path.to_path_buf(),
    };
    config
        .rules
        .into_iter()
        .map(|rule| Sourced {
            value: rule,
            source: source.clone(),
        })
        .collect()
}

pub fn json_env_flag(env: Option<&serde_json::Value>, key: &str) -> Option<bool> {
    let val = env?.get(key)?;
    match val {
        serde_json::Value::Number(n) => Some(n.as_i64().unwrap_or(0) != 0),
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => match s.as_str() {
            "0" | "" | "false" => Some(false),
            _ => Some(true),
        },
        _ => None,
    }
}

fn parse_disable_bypass_permissions(json: &serde_json::Value) -> Option<bool> {
    let val = json
        .get("permissions")?
        .get("disableBypassPermissionsMode")?;
    Some(val.as_str() == Some("disable"))
}

/// Whether vendor `managed-settings.json` requests Claude's bypass lock. Advisory only: the resolver ignores it so grok does not inherit a host-wide lockdown.
/// Render as `claudeBypassLockAdvisory`, never as enforced policy.
pub fn claude_bypass_lock_request(features: &ManagedSettingsFeatures) -> bool {
    features.source_path.is_some() && features.disable_yolo == Some(true)
}

/// Which requirements key activated the always-approve hard lock
/// ([`yolo_disabled_by_policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum YoloPinReason {
    /// `[ui] disable_bypass_permissions_mode = true` in requirements.toml.
    DisableBypassPermissionsMode,
    /// Back-compat: the legacy `[ui] yolo = false` requirements key still locks.
    LegacyYoloFalse,
}

impl YoloPinReason {
    /// Admin-facing pin message, shown in launch warnings and skip reasons.
    pub const fn message(self) -> &'static str {
        match self {
            Self::DisableBypassPermissionsMode => {
                "always-approve disabled by managed policy ([ui] disable_bypass_permissions_mode = true in requirements.toml)"
            }
            Self::LegacyYoloFalse => {
                "always-approve disabled by managed policy ([ui] yolo = false in requirements.toml)"
            }
        }
    }
}

/// The active always-approve hard lock: the pin reason plus the label of the requirements layer that set it.
/// The label is a file path, or the diskless macOS MDM source id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoloPolicyLock {
    pub source_label: String,
    pub reason: YoloPinReason,
}

/// Hard-lock predicate. `Some(reason)` iff a requirements layer sets `[ui] disable_bypass_permissions_mode` (or legacy `[ui] yolo = false`).
/// Vendor `disableBypassPermissionsMode` is not consulted, so grok does not inherit a host-wide lockdown; use root-owned `requirements.toml`. Fails open on user-writable layers.
pub fn yolo_disabled_by_policy() -> Option<&'static str> {
    yolo_policy_lock().map(|lock| lock.reason.message())
}

/// Layer-attributed form of [`yolo_disabled_by_policy`] so provenance displays can name the pinning layer.
pub fn yolo_policy_lock() -> Option<YoloPolicyLock> {
    let layers = distill_config::requirements_layers();
    // Owned so the label outlives the borrowed layer temporaries below.
    let labeled: Vec<(PathBuf, &toml::Value)> = layers
        .iter()
        .map(|l| (PathBuf::from(l.source.label().as_ref()), &l.value))
        .collect();
    resolve_yolo_policy_block(labeled.iter().map(|(p, v)| (p.as_path(), *v)))
}

/// Read `[ui] <key>` as a bool; a non-bool value warns (naming key and layer) rather than silently failing to lock.
fn requirements_lock_bool(ui: Option<&toml::Value>, key: &str, path: &Path) -> Option<bool> {
    let value = ui?.get(key)?;
    match value.as_bool() {
        Some(b) => Some(b),
        None => {
            warn!(
                path = %path.display(),
                key,
                "[ui] {key} must be a boolean; ignoring non-bool value \
                 (always-approve lock not applied from this key in this layer)"
            );
            None
        }
    }
}

/// Pure form of [`yolo_policy_lock`] over pre-loaded layers; `path` labels the lock's provenance and non-bool warnings.
fn resolve_yolo_policy_block<'a>(
    requirement_layers: impl Iterator<Item = (&'a Path, &'a toml::Value)>,
) -> Option<YoloPolicyLock> {
    let lock = |path: &Path, reason| {
        Some(YoloPolicyLock {
            source_label: path.display().to_string(),
            reason,
        })
    };
    for (path, layer) in requirement_layers {
        let ui = layer.get("ui");
        // Native lock key (default false). `true` pins always-approve off.
        if requirements_lock_bool(ui, "disable_bypass_permissions_mode", path) == Some(true) {
            return lock(path, YoloPinReason::DisableBypassPermissionsMode);
        }
        // Back-compat alias: `[ui] yolo = false` in requirements.toml still pins (pre-rename configs)
        // A config.toml `yolo` is unaffected (not read here)
        if requirements_lock_bool(ui, "yolo", path) == Some(false) {
            return lock(path, YoloPinReason::LegacyYoloFalse);
        }
    }
    None
}

#[cfg(test)]
#[path = "resolution_tests.rs"]
mod tests;
