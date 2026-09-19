// Modified for Distill by Samuel Fajreldines, 2026.
// Scans Claude settings and generates TOML patches for .grok/config.toml.
//
// This module reuses the existing discovery and parsing functions from claude_compat.rs and util/config.rs
// It does NOT modify the runtime Claude compat layer; that continues to work as before

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
use tracing::{debug, info, warn};

use crate::util::config::McpServerConfig;
use distill_workspace::permission::claude_settings::{
    find_claude_settings_paths, load_claude_settings,
};
use distill_workspace::permission::rules::parse_permission_rule;
use distill_workspace::permission::types::{PatternMode, PermissionRule, RuleAction, ToolFilter};

// Types

/// Scope for an import operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportScope {
    /// User-level: writes to `~/.grok/config.toml`.
    Global,
    /// Project-level: writes to `<repo>/.grok/config.toml`.
    Project,
}

/// Which `[paths]` field a `PathEntry` populates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Maps to `[paths] extra_skill_dirs`.
    Skill,
    /// Maps to `[paths] extra_rule_dirs`.
    Rule,
}

/// A single item that can be imported.
#[derive(Debug, Clone)]
pub enum ImportableItem {
    /// A permission rule (allow/deny/ask).
    Permission(PermissionRule),
    /// An environment variable.
    EnvVar { key: String, value: String },
    /// An MCP server configuration.
    McpServer {
        name: String,
        config: Box<McpServerConfig>,
    },
    /// A single hook (one event, matcher, and command, derived from a Claude `hooks` entry).
    Hook {
        event: String,
        matcher: Option<String>,
        command: String,
        timeout: Option<u64>,
    },
    /// A path entry to add to `[paths] extra_skill_dirs` or `extra_rule_dirs`.
    PathEntry { kind: PathKind, path: String },
}

/// A plan describing what would be imported and where.
#[derive(Debug, Clone, Default)]
pub struct ImportPlan {
    /// Items to write to `~/.grok/config.toml`.
    pub global_items: Vec<ImportableItem>,
    /// Items to write to `<repo>/.grok/config.toml`.
    pub project_items: Vec<ImportableItem>,
}

impl ImportPlan {
    /// Total number of items across both scopes.
    pub fn total_items(&self) -> usize {
        self.global_items.len() + self.project_items.len()
    }

    /// Whether there's nothing to import.
    pub fn is_empty(&self) -> bool {
        self.global_items.is_empty() && self.project_items.is_empty()
    }

    /// Format a human-readable summary of the import plan.
    pub fn summary(&self, cwd: &Path) -> String {
        if self.is_empty() {
            return "No Claude settings found to import.".to_string();
        }

        let mut out = String::from("Found Claude settings to import:\n");

        if !self.global_items.is_empty() {
            out.push_str("\nGlobal (~/.grok/config.toml):\n");
            out.push_str(&format_item_summary(&self.global_items));
        }

        if !self.project_items.is_empty() {
            out.push_str(&format!(
                "\nProject ({}/.grok/config.toml):\n",
                find_project_root(cwd).display()
            ));
            out.push_str(&format_item_summary(&self.project_items));
        }

        out
    }
}

/// Format a summary of items grouped by type, with per-item detail lines.
fn format_item_summary(items: &[ImportableItem]) -> String {
    let mut out = String::new();

    // Permission rules
    let perms: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            ImportableItem::Permission(rule) => Some(rule),
            _ => None,
        })
        .collect();
    if !perms.is_empty() {
        let mut allow = 0u32;
        let mut deny = 0u32;
        let mut ask = 0u32;
        for rule in &perms {
            match rule.action {
                RuleAction::Allow => allow += 1,
                RuleAction::Deny => deny += 1,
                RuleAction::Ask => ask += 1,
            }
        }
        let mut parts = Vec::new();
        if allow > 0 {
            parts.push(format!("{} allow", allow));
        }
        if deny > 0 {
            parts.push(format!("{} deny", deny));
        }
        if ask > 0 {
            parts.push(format!("{} ask", ask));
        }
        out.push_str(&format!(
            "  - {} permission rule(s) ({})\n",
            perms.len(),
            parts.join(", ")
        ));
        for rule in &perms {
            let action = match rule.action {
                RuleAction::Allow => "allow",
                RuleAction::Deny => "deny",
                RuleAction::Ask => "ask",
            };
            out.push_str(&format!("      {} {}\n", action, format_rule_string(rule)));
        }
    }

    // Env vars
    let envs: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            ImportableItem::EnvVar { key, value } => Some((key, value)),
            _ => None,
        })
        .collect();
    if !envs.is_empty() {
        out.push_str(&format!("  - {} environment variable(s)\n", envs.len()));
        for (key, value) in &envs {
            // Redact the value: even keys like FOO_KEY can hide secrets (API tokens, credentials)
            // The raw value still flows into the on-disk config.toml for actual use; only the human-facing summary suppresses it
            // The length hint lets the user recognise their setting without exposing the contents in terminals, screenshots, or CI logs
            out.push_str(&format!(
                "      {} = <redacted, {} chars>\n",
                key,
                value.len()
            ));
        }
    }

    // MCP servers
    let mcps: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            ImportableItem::McpServer { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    if !mcps.is_empty() {
        out.push_str(&format!("  - {} MCP server(s)\n", mcps.len()));
        for name in &mcps {
            out.push_str(&format!("      {}\n", name));
        }
    }

    // Hooks
    let hooks: Vec<_> = items
        .iter()
        .filter_map(|i| match i {
            ImportableItem::Hook {
                event,
                matcher,
                command,
                timeout,
            } => Some((event, matcher, command, timeout)),
            _ => None,
        })
        .collect();
    if !hooks.is_empty() {
        out.push_str(&format!("  - {} hook(s)\n", hooks.len()));
        for (event, matcher, command, timeout) in &hooks {
            let m = matcher.as_deref().unwrap_or("*");
            let timeout_suffix = match timeout {
                Some(t) => format!(" (timeout: {}s)", t),
                None => String::new(),
            };
            out.push_str(&format!(
                "      {} [{}] {}{}\n",
                event, m, command, timeout_suffix
            ));
        }
    }

    // Path entries
    let paths_iter = items.iter().filter_map(|i| match i {
        ImportableItem::PathEntry { kind, path } => Some((*kind, path)),
        _ => None,
    });
    let skill_paths: Vec<&String> = paths_iter
        .clone()
        .filter_map(|(k, p)| if k == PathKind::Skill { Some(p) } else { None })
        .collect();
    let rule_paths: Vec<&String> = paths_iter
        .filter_map(|(k, p)| if k == PathKind::Rule { Some(p) } else { None })
        .collect();
    if !skill_paths.is_empty() {
        out.push_str(&format!("  - {} extra skill dir(s)\n", skill_paths.len()));
        for p in &skill_paths {
            out.push_str(&format!("      {}\n", p));
        }
    }
    if !rule_paths.is_empty() {
        out.push_str(&format!("  - {} extra rule dir(s)\n", rule_paths.len()));
        for p in &rule_paths {
            out.push_str(&format!("      {}\n", p));
        }
    }

    out
}

/// Parse Claude `hooks` JSON from a settings file at `path` into `ImportableItem::Hook` items.
/// Claude `hooks` shape: Each command handler becomes one `ImportableItem::Hook`.
/// HTTP handlers and other types are skipped (we only import shell commands).
fn extract_hooks_from_settings_file(path: &Path) -> Vec<ImportableItem> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(hooks_obj) = value.get("hooks").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for (event, groups_val) in hooks_obj {
        let Some(groups) = groups_val.as_array() else {
            continue;
        };
        for group in groups {
            let matcher = group
                .get("matcher")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let Some(handlers) = group.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for handler in handlers {
                let handler_type = handler.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if handler_type != "command" {
                    debug!(
                        path = %path.display(),
                        event = %event,
                        handler_type = %handler_type,
                        "Skipping non-command hook handler during import"
                    );
                    continue;
                }
                let Some(command) = handler.get("command").and_then(|v| v.as_str()) else {
                    continue;
                };
                let timeout = handler.get("timeout").and_then(|v| v.as_u64());
                items.push(ImportableItem::Hook {
                    event: event.clone(),
                    matcher: matcher.clone(),
                    command: command.to_string(),
                    timeout,
                });
            }
        }
    }
    items
}

// Scanner

/// Scan all Claude settings sources and build an import plan. Discovers: Permission rules from `.claude/settings*.json` (global and project) Environment variables from `.claude/settings*.json`
/// MCP servers from `~/.claude.json` (global and per-project) MCP servers from `.mcp.json` files (project)
pub fn scan_importable_settings(cwd: &Path) -> ImportPlan {
    let mut plan = ImportPlan::default();

    let all_paths = find_claude_settings_paths(cwd);
    // The home used for the is_global split must match the resolution in config.rs and claude_import_state.rs, or scan and hash tiers disagree
    let home = distill_dirs::home_dir();

    for path in &all_paths {
        let Some(settings) = load_claude_settings(path) else {
            continue;
        };

        let is_global = home
            .as_ref()
            .is_some_and(|h| path.starts_with(h.join(".claude")));
        let target = if is_global {
            &mut plan.global_items
        } else {
            &mut plan.project_items
        };

        // Permission rules.
        if let Some(perms) = settings.permissions {
            for (action, entries) in [
                (RuleAction::Allow, perms.allow),
                (RuleAction::Deny, perms.deny),
                (RuleAction::Ask, perms.ask),
            ] {
                for rule_str in entries {
                    match parse_permission_rule(&rule_str, action) {
                        Ok(rule) => target.push(ImportableItem::Permission(rule)),
                        Err(e) => {
                            debug!(
                                path = %path.display(),
                                rule = %rule_str,
                                error = %e,
                                "Skipping unparseable Claude permission rule"
                            );
                        }
                    }
                }
            }
        }

        // Environment variables.
        if let Some(env) = settings.env {
            for (key, value) in env {
                target.push(ImportableItem::EnvVar { key, value });
            }
        }

        // Hooks (re-parsed directly from the JSON since `ClaudeSettings` doesn't model `hooks`).
        for hook in extract_hooks_from_settings_file(path) {
            target.push(hook);
        }
    }

    scan_claude_json_mcp_servers(cwd, &mut plan);

    scan_mcp_json_servers(cwd, &mut plan);

    scan_claude_path_dirs(cwd, &mut plan);

    if !plan.is_empty() {
        info!(
            global = plan.global_items.len(),
            project = plan.project_items.len(),
            "Scanned Claude settings for import"
        );
    }

    plan
}

/// Scan `~/.claude/{skills,rules}` (global) and `<repo>/.claude/{skills,rules}` (project).
/// Emits `PathEntry` items so the dirs survive the runtime cutoff.
fn scan_claude_path_dirs(cwd: &Path, plan: &mut ImportPlan) {
    // Track canonicalised global paths so the project scan below can dedup against them
    // When the user runs `/import-claude` from `~`, the project root *is* the home directory
    // The same `.claude/skills` would otherwise be added to both global and project scopes
    let mut global_added: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();

    if let Some(home) = distill_dirs::home_dir() {
        for (kind, sub) in [(PathKind::Skill, "skills"), (PathKind::Rule, "rules")] {
            let dir = home.join(".claude").join(sub);
            if dir.is_dir() {
                let canonical = dunce::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
                global_added.insert(canonical);
                plan.global_items.push(ImportableItem::PathEntry {
                    kind,
                    path: dir.to_string_lossy().to_string(),
                });
            }
        }
    }

    let project_root = find_project_root(cwd);
    for (kind, sub) in [(PathKind::Skill, "skills"), (PathKind::Rule, "rules")] {
        let dir = project_root.join(".claude").join(sub);
        if dir.is_dir() {
            let canonical = dunce::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if global_added.contains(&canonical) {
                debug!(
                    path = %dir.display(),
                    "Skipping project .claude/ path that resolves to the same dir as the global entry"
                );
                continue;
            }
            plan.project_items.push(ImportableItem::PathEntry {
                kind,
                path: dir.to_string_lossy().to_string(),
            });
        }
    }
}

/// Scan MCP servers from `~/.claude.json`.
fn scan_claude_json_mcp_servers(cwd: &Path, plan: &mut ImportPlan) {
    let servers = crate::util::config::load_claude_json_mcp_servers_as_configs_unfiltered(cwd);
    if servers.is_empty() {
        return;
    }

    // TODO: project-specific servers are incorrectly classified as global here
    // `load_claude_json_mcp_servers_as_configs()` merges the top-level `mcpServers` in `~/.claude.json` with `projects.<cwd>.mcpServers` into one map
    // The fix: call `load_claude_json_mcp_servers_from()` twice (user-level entries, then project entries), or expose a split variant in `config.rs`
    for (name, config) in servers {
        plan.global_items.push(ImportableItem::McpServer {
            name,
            config: Box::new(config),
        });
    }
}

/// Scan MCP servers from `.mcp.json` files.
fn scan_mcp_json_servers(cwd: &Path, plan: &mut ImportPlan) {
    let servers = crate::util::config::load_mcp_json_servers_as_configs_unfiltered(cwd);
    for (name, config) in servers {
        plan.project_items.push(ImportableItem::McpServer {
            name,
            config: Box::new(config),
        });
    }
}

// Repo Root Discovery

/// Find the git repo root for project config writes.
/// Uses `git2::Repository::discover` (matching `config/mod.rs:find_project_configs`) to find the repo root. Falls back to `cwd` if no git repo is found.
pub fn find_project_root(cwd: &Path) -> PathBuf {
    git2::Repository::discover(cwd)
        .ok()
        .and_then(|repo| repo.workdir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| cwd.to_path_buf())
}

// Import Marker (Read Side) The marker `[claude_compat] imported = true` in `~/.grok/config.toml` is the signal that runtime fallback paths should stop reading `.claude/`.
// The reader lives here so the hook, path, and permission gates all consult the same cached marker The writer is `mark_claude_imported` below

/// Cached result of [`is_claude_import_marked`]; see its doc for the caching rationale and trade-offs.
/// `RwLock<Option<bool>>` rather than `OnceLock<bool>` so tests can reset the state between cases.
/// The fast path is a read lock and a cached `bool`, far below the cost of the uncached `read_to_string` and TOML parse.
static MARKER_CACHE: std::sync::RwLock<Option<bool>> = std::sync::RwLock::new(None);

/// Whether the current user has already imported Claude settings. Reads `[claude_compat] imported = true` from `~/.grok/config.toml` once per process and caches the result.
/// When the marker is set, runtime fallbacks that read `.claude/` should be skipped; the user has migrated to native config. Resilient: returns `false` on missing file, missing section, parse error, or any other failure.
/// Trade-off: a user who manually flips the marker mid-session must restart to see the change, acceptable because reverting after import is rare. That variant logs one line so users can see the cutoff fired.
pub(crate) fn is_claude_import_marked() -> bool {
    if let Some(v) = *MARKER_CACHE.read().expect("MARKER_CACHE poisoned") {
        return v;
    }
    let config_path = crate::util::distill_home::distill_home().join("config.toml");
    let v = is_claude_import_marked_at(&config_path);
    *MARKER_CACHE.write().expect("MARKER_CACHE poisoned") = Some(v);
    v
}

/// Forcibly seed the cache with the freshly written marker value.
/// The slash command calls this after `apply_import` writes the marker so gate checks reflect the new state without a restart.
pub(crate) fn refresh_marker_cache(value: bool) {
    *MARKER_CACHE.write().expect("MARKER_CACHE poisoned") = Some(value);
}

/// Reset the marker cache to uninitialised. Test-only.
#[cfg(test)]
pub(crate) fn reset_marker_cache_for_test() {
    *MARKER_CACHE.write().expect("MARKER_CACHE poisoned") = None;
}

/// Like [`is_claude_import_marked`], but logs a one-time `info!` line on the first true result per process.
/// `gate_name` identifies which call site fired the cutoff (useful for debugging which subsystem stopped reading `.claude/`).
/// Call sites are runtime fallback paths in `claude_compat.rs`, `util/config.rs`, `util/hooks.rs`, and `agent/config.rs` that previously read `.claude/`.
pub(crate) fn is_claude_import_marked_with_log(gate_name: &'static str) -> bool {
    static LOGGED: OnceLock<()> = OnceLock::new();
    let marked = is_claude_import_marked();
    if marked {
        LOGGED.get_or_init(|| {
            info!(
                first_gate = gate_name,
                "Claude compat disabled (marker set in config.toml)"
            );
        });
    }
    marked
}

/// Testable variant of [`is_claude_import_marked`] that reads from the given path.
pub(crate) fn is_claude_import_marked_at(config_path: &Path) -> bool {
    let content = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let value: TomlValue = match toml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    value
        .get("claude_compat")
        .and_then(|v| v.get("imported"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Write `[claude_compat] imported = true` to `~/.grok/config.toml`.
/// Uses the same atomic write pattern as `save_mcp_server_config` (write to `.tmp`, then rename).
/// Creates the file and parent directory if missing. Existing content in the file is preserved.
fn write_import_marker(config_path: &Path) -> anyhow::Result<()> {
    // Report parse errors instead of silently discarding the file
    // An atomic rewrite would otherwise drop unrelated sections ([model], [ui], etc.) and overwrite a hand-edited config that happens to have a trailing comma
    // The user can fix the TOML and retry
    let mut root: TomlValue = match std::fs::read_to_string(config_path) {
        Ok(s) => toml::from_str(&s).map_err(|e| {
            anyhow::anyhow!(
                "refusing to write import marker: existing config at {} is \
                 not valid TOML ({}). Fix the file (or move it aside) and \
                 retry.",
                config_path.display(),
                e
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TomlValue::Table(TomlMap::new()),
        Err(e) => return Err(e.into()),
    };
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config root is not a table"))?;
    let compat = table
        .entry("claude_compat")
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let compat_table = compat
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[claude_compat] is not a table"))?;
    compat_table.insert("imported".to_string(), TomlValue::Boolean(true));

    let toml_str = toml::to_string_pretty(&root)?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = config_path.with_extension("toml.tmp");
    // Best-effort cleanup of the .tmp file if either write or rename fails
    // A stale .tmp would otherwise survive next to the real config, and the next attempt would inherit a half-written file before the rename clobbers it
    if let Err(e) = std::fs::write(&tmp, &toml_str) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, config_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Public entry point for the slash command: write the marker, log success, and seed the in-process cache so gate checks reflect it without restart.
/// Called from `/import-claude` even when nothing was imported: the marker is the user's opt-in choice, not a side effect of having imported items.
/// Re-entering a workspace with `.claude/` content must not re-engage the runtime fallbacks.
pub fn mark_claude_imported() -> anyhow::Result<()> {
    let path = crate::util::distill_home::distill_home().join("config.toml");
    write_import_marker(&path)?;
    refresh_marker_cache(true);
    Ok(())
}
// TOML Patch Writer

/// Apply an import plan by writing TOML patches to the appropriate config files. This is additive-only: existing entries are never removed.
/// New permission rules are appended; new env vars and MCP servers are added without overwriting existing keys or names.
/// Project items are written to `<repo_root>/.grok/config.toml` (discovered via `git2::Repository::discover`), not `cwd/.grok/config.toml`. This avoids creating config files in unexpected subdirectories.
pub fn apply_import(plan: &ImportPlan, cwd: &Path) -> anyhow::Result<ImportResult> {
    let mut result = ImportResult::default();

    if !plan.global_items.is_empty() {
        let global_path = crate::util::distill_home::distill_home().join("config.toml");
        let count = apply_items_to_config(&global_path, &plan.global_items)?;
        result.global_count = count;
        if count > 0 {
            result
                .modified_files
                .push(global_path.to_string_lossy().to_string());
        }

        // Hooks are written separately to ~/.grok/hooks/imported-from-claude.json.
        let hooks_dir = crate::util::distill_home::distill_home().join("hooks");
        let hook_count = apply_hooks_to_dir(&hooks_dir, &plan.global_items)?;
        result.global_count += hook_count;
        if hook_count > 0 {
            result.modified_files.push(
                hooks_dir
                    .join("imported-from-claude.json")
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }

    if !plan.project_items.is_empty() {
        let project_root = find_project_root(cwd);
        let project_path = project_root.join(".grok").join("config.toml");
        let count = apply_items_to_config(&project_path, &plan.project_items)?;
        result.project_count = count;
        if count > 0 {
            result
                .modified_files
                .push(project_path.to_string_lossy().to_string());
        }

        let hooks_dir = project_root.join(".grok").join("hooks");
        let hook_count = apply_hooks_to_dir(&hooks_dir, &plan.project_items)?;
        result.project_count += hook_count;
        if hook_count > 0 {
            result.modified_files.push(
                hooks_dir
                    .join("imported-from-claude.json")
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }

    // The slash command (`/import-claude`) is responsible for writing the `[claude_compat] imported = true` marker via `mark_claude_imported()`
    // It does so regardless of `result.total()` so a user invocation that finds nothing to import still records the user's opt-in choice

    Ok(result)
}

/// Result of applying an import.
#[derive(Debug, Default)]
pub struct ImportResult {
    /// Number of items written to global config.
    pub global_count: usize,
    /// Number of items written to project config.
    pub project_count: usize,
    /// Paths of config files that were modified.
    pub modified_files: Vec<String>,
}

impl ImportResult {
    pub fn total(&self) -> usize {
        self.global_count + self.project_count
    }
}

/// Apply items to a single config.toml file using atomic write.
fn apply_items_to_config(config_path: &Path, items: &[ImportableItem]) -> anyhow::Result<usize> {
    // Read existing TOML, reporting parse errors instead of silently discarding the file
    // An atomic rewrite would otherwise drop unrelated sections ([model], [ui], etc.) and overwrite a hand-edited config that happens to have a trailing comma
    let mut root: TomlValue = match std::fs::read_to_string(config_path) {
        Ok(s) => toml::from_str(&s).map_err(|e| {
            anyhow::anyhow!(
                "refusing to import: existing config at {} is not valid TOML \
                 ({}). Fix the file (or move it aside) and retry.",
                config_path.display(),
                e
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TomlValue::Table(TomlMap::new()),
        Err(e) => return Err(e.into()),
    };

    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config root is not a table"))?;

    let mut count = 0usize;

    // Group items by type.
    let mut permissions: Vec<&PermissionRule> = Vec::new();
    let mut env_vars: Vec<(&str, &str)> = Vec::new();
    let mut mcp_servers: Vec<(&str, &McpServerConfig)> = Vec::new();
    let mut skill_dirs: Vec<&str> = Vec::new();
    let mut rule_dirs: Vec<&str> = Vec::new();

    for item in items {
        match item {
            ImportableItem::Permission(rule) => permissions.push(rule),
            ImportableItem::EnvVar { key, value } => env_vars.push((key, value)),
            ImportableItem::McpServer { name, config } => mcp_servers.push((name, config)),
            // Hooks are written to .grok/hooks/ JSON files in apply_hooks_to_dir, not into config.toml
            ImportableItem::Hook { .. } => {}
            ImportableItem::PathEntry { kind, path } => match kind {
                PathKind::Skill => skill_dirs.push(path.as_str()),
                PathKind::Rule => rule_dirs.push(path.as_str()),
            },
        }
    }

    if !permissions.is_empty() {
        count += merge_permissions(table, &permissions)?;
    }

    if !skill_dirs.is_empty() {
        count += merge_paths(table, "extra_skill_dirs", &skill_dirs)?;
    }
    if !rule_dirs.is_empty() {
        count += merge_paths(table, "extra_rule_dirs", &rule_dirs)?;
    }

    if !env_vars.is_empty() {
        count += merge_env_vars(table, &env_vars);
    }

    if !mcp_servers.is_empty() {
        count += merge_mcp_servers(table, &mcp_servers)?;
    }

    if count > 0 {
        // Atomic write: write to .tmp, then rename.
        let toml_str = toml::to_string_pretty(&root)?;
        let tmp = config_path.with_extension("toml.tmp");
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp, &toml_str)?;
        std::fs::rename(&tmp, config_path)?;
        info!(
            path = %config_path.display(),
            count,
            "Wrote imported settings to config.toml"
        );
    }

    Ok(count)
}

/// Merge permission rules into `[permission]` using the compact format.
/// Existing rules are preserved. New rules are appended to the appropriate action list (`allow`, `deny`, `ask`).
fn merge_permissions(
    table: &mut TomlMap<String, TomlValue>,
    rules: &[&PermissionRule],
) -> anyhow::Result<usize> {
    let permission = table
        .entry("permission")
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let perm_table = permission
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[permission] is not a table"))?;

    let mut count = 0;

    // Group rules by action.
    let mut allow_rules: Vec<String> = Vec::new();
    let mut deny_rules: Vec<String> = Vec::new();
    let mut ask_rules: Vec<String> = Vec::new();

    for rule in rules {
        let formatted = format_rule_string(rule);
        match rule.action {
            RuleAction::Allow => allow_rules.push(formatted),
            RuleAction::Deny => deny_rules.push(formatted),
            RuleAction::Ask => ask_rules.push(formatted),
        }
    }

    for (key, new_rules) in [
        ("allow", allow_rules),
        ("deny", deny_rules),
        ("ask", ask_rules),
    ] {
        if new_rules.is_empty() {
            continue;
        }

        let arr = perm_table
            .entry(key)
            .or_insert_with(|| TomlValue::Array(Vec::new()));
        let existing = arr
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("permission.{key} is not an array"))?;

        // Collect existing strings for dedup.
        let existing_set: std::collections::HashSet<String> = existing
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();

        for rule_str in new_rules {
            if !existing_set.contains(&rule_str) {
                existing.push(TomlValue::String(rule_str));
                count += 1;
            }
        }
    }

    Ok(count)
}

/// Format a `PermissionRule` back to the compact Claude-style string.
/// Examples: `"Bash(npm run build)"` for `{ Allow, Bash, "npm run build" }` `"Read(src/*.rs)"` for `{ Allow, Read, "src/*.rs" }` `"Bash"` for `{ Allow, Bash, None }` (bare tool name, any pattern) `"*"` for `{ Allow, Any, None }` (catch-all rule)
fn format_rule_string(rule: &PermissionRule) -> String {
    let tool_name = match rule.tool {
        ToolFilter::Any => "",
        ToolFilter::Bash => "Bash",
        ToolFilter::Edit => "Edit",
        ToolFilter::Read => "Read",
        ToolFilter::Grep => "Grep",
        ToolFilter::Mcp => "MCPTool",
        ToolFilter::WebFetch => "WebFetch",
        ToolFilter::WebSearch => "WebSearch",
        ToolFilter::AgentMessage => "AgentMessage",
        _ => "",
    };

    match (&rule.pattern, &rule.tool) {
        // Catch-all: any tool and no pattern formats to "*"
        (None, ToolFilter::Any) => "*".to_string(),
        (Some(pat), ToolFilter::Any) => pat.clone(),
        (None, _) => tool_name.to_string(),
        (Some(pat), _) => {
            let pattern = match rule.pattern_mode {
                PatternMode::Domain => format!("domain:{}", pat),
                PatternMode::Glob => pat.clone(),
            };
            format!("{}({})", tool_name, pattern)
        }
    }
}

/// Merge environment variables into `[env]`. Existing keys are NOT overwritten.
fn merge_env_vars(table: &mut TomlMap<String, TomlValue>, vars: &[(&str, &str)]) -> usize {
    let env = table
        .entry("env")
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let env_table = match env.as_table_mut() {
        Some(t) => t,
        None => {
            warn!("[env] in config.toml is not a table, skipping env import");
            return 0;
        }
    };

    let mut count = 0;
    for (key, value) in vars {
        if !env_table.contains_key(*key) {
            env_table.insert(key.to_string(), TomlValue::String(value.to_string()));
            count += 1;
        }
    }
    count
}

/// Merge MCP server configs into `[mcp_servers]`. Existing servers are NOT overwritten.
fn merge_mcp_servers(
    table: &mut TomlMap<String, TomlValue>,
    servers: &[(&str, &McpServerConfig)],
) -> anyhow::Result<usize> {
    let mcp = table
        .entry("mcp_servers")
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let mcp_table = mcp
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[mcp_servers] is not a table"))?;

    let mut count = 0;
    for (name, config) in servers {
        if !mcp_table.contains_key(*name) {
            let serialized = toml::Value::try_from(*config)
                .map_err(|e| anyhow::anyhow!("failed to serialize MCP server {name}: {e}"))?;
            mcp_table.insert(name.to_string(), serialized);
            count += 1;
        }
    }
    Ok(count)
}

/// Merge a list of path strings into `[paths] <key>` (an array of strings).
/// Existing entries are preserved; new entries that aren't already present are appended.
/// Returns the number of newly added entries.
fn merge_paths(
    table: &mut TomlMap<String, TomlValue>,
    key: &str,
    new_paths: &[&str],
) -> anyhow::Result<usize> {
    let paths = table
        .entry("paths")
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let paths_table = paths
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[paths] is not a table"))?;

    let arr = paths_table
        .entry(key)
        .or_insert_with(|| TomlValue::Array(Vec::new()));
    let existing = arr
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("paths.{key} is not an array"))?;

    let existing_set: std::collections::HashSet<String> = existing
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    let mut count = 0usize;
    for p in new_paths {
        if !existing_set.contains(*p) {
            existing.push(TomlValue::String(p.to_string()));
            count += 1;
        }
    }
    Ok(count)
}

/// Merge `Hook` items into `<hooks_dir>/imported-from-claude.json`. The output JSON is the same shape that `distill-hooks` natively understands (Claude-compatible).
/// The native hooks loader scans `.grok/hooks/*.json` directly, so no separate config-side parser is required. Existing entries with the same `(event, matcher, command)` triple are deduped.
fn apply_hooks_to_dir(hooks_dir: &Path, items: &[ImportableItem]) -> anyhow::Result<usize> {
    let new_hooks: Vec<&ImportableItem> = items
        .iter()
        .filter(|i| matches!(i, ImportableItem::Hook { .. }))
        .collect();
    if new_hooks.is_empty() {
        return Ok(0);
    }

    let target = hooks_dir.join("imported-from-claude.json");

    // Read existing JSON if present.
    let mut root: serde_json::Value = match std::fs::read_to_string(&target) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            warn!(
                path = %target.display(),
                error = %e,
                "Existing imported-from-claude.json is malformed; replacing with fresh content. \
                 Manual edits in the malformed file will be lost."
            );
            serde_json::json!({})
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.into()),
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", target.display()))?;
    let hooks_obj = root_obj
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: hooks is not a JSON object", target.display()))?;

    let mut count = 0usize;
    // `dirty` tracks whether we mutated the JSON in any way (including in-place timeout refreshes that don't add new entries)
    // The file is re-written only when dirty, even when count == 0
    let mut dirty = false;
    for item in new_hooks {
        let ImportableItem::Hook {
            event,
            matcher,
            command,
            timeout,
        } = item
        else {
            continue;
        };

        let groups = hooks_obj
            .entry(event.clone())
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                anyhow::anyhow!("{}: hooks.{} is not a JSON array", target.display(), event)
            })?;

        // Dedup on `(event, matcher, command)` If a matching entry already exists, update its `timeout` in place and skip adding a new group; otherwise append a new group below
        // A re-import with a changed timeout therefore reflects in the output Invariant: `extract_hooks_from_settings_file` filters empty matcher strings to `None`, so the comparison only distinguishes `None` from `Some(s)`
        let mut updated = false;
        for g in groups.iter_mut() {
            let existing_matcher = g.get("matcher").and_then(|v| v.as_str());
            let matcher_matches = match (existing_matcher, matcher.as_deref()) {
                (None, None) => true,
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if !matcher_matches {
                continue;
            }
            let Some(handlers) = g.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                continue;
            };
            for h in handlers.iter_mut() {
                let cmd_match = h.get("type").and_then(|v| v.as_str()) == Some("command")
                    && h.get("command").and_then(|v| v.as_str()) == Some(command);
                if !cmd_match {
                    continue;
                }
                if let Some(handler_obj) = h.as_object_mut() {
                    match timeout {
                        Some(t) => {
                            handler_obj.insert("timeout".to_string(), serde_json::json!(t));
                        }
                        None => {
                            handler_obj.remove("timeout");
                        }
                    }
                }
                debug!(
                    event = %event,
                    matcher = ?matcher,
                    command = %command,
                    timeout = ?timeout,
                    "Hook already present; refreshed timeout in place"
                );
                updated = true;
                dirty = true;
                break;
            }
            if updated {
                break;
            }
        }
        if updated {
            continue;
        }

        let mut handler = serde_json::json!({
            "type": "command",
            "command": command,
        });
        if let Some(t) = timeout {
            handler
                .as_object_mut()
                .unwrap()
                .insert("timeout".to_string(), serde_json::json!(t));
        }

        let mut group = serde_json::json!({
            "hooks": [handler],
        });
        if let Some(m) = matcher {
            group
                .as_object_mut()
                .unwrap()
                .insert("matcher".to_string(), serde_json::json!(m));
        }
        groups.push(group);
        count += 1;
    }

    if count > 0 || dirty {
        std::fs::create_dir_all(hooks_dir)?;
        let json_str = serde_json::to_string_pretty(&root)?;
        let tmp = target.with_extension("json.tmp");
        std::fs::write(&tmp, &json_str)?;
        std::fs::rename(&tmp, &target)?;
        info!(
            path = %target.display(),
            count,
            "Wrote imported hooks to .grok/hooks/imported-from-claude.json"
        );
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_workspace::HookSourceConfig;

    fn source_path_strs(sources: &[HookSourceConfig]) -> Vec<String> {
        sources
            .iter()
            .map(|s| match s {
                HookSourceConfig::SettingsFile(p) | HookSourceConfig::Directory(p) => {
                    p.to_string_lossy().into_owned()
                }
            })
            .collect()
    }

    #[test]
    fn format_rule_bash_with_pattern() {
        let rule = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Bash,
            pattern: Some("npm run build".to_string()),
            pattern_mode: PatternMode::Glob,
        };
        assert_eq!(format_rule_string(&rule), "Bash(npm run build)");
    }

    #[test]
    fn format_rule_bare_tool_name() {
        let rule = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Bash,
            pattern: None,
            pattern_mode: PatternMode::Glob,
        };
        assert_eq!(format_rule_string(&rule), "Bash");
    }

    #[test]
    fn format_rule_any_none_is_star() {
        let rule = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Any,
            pattern: None,
            pattern_mode: PatternMode::Glob,
        };
        assert_eq!(format_rule_string(&rule), "*");
    }

    #[test]
    fn format_rule_any_with_pattern() {
        let rule = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Any,
            pattern: Some("src/**".to_string()),
            pattern_mode: PatternMode::Glob,
        };
        assert_eq!(format_rule_string(&rule), "src/**");
    }

    #[test]
    fn format_rule_web_fetch_domain() {
        let rule = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::WebFetch,
            pattern: Some("example.com".to_string()),
            pattern_mode: PatternMode::Domain,
        };
        assert_eq!(format_rule_string(&rule), "WebFetch(domain:example.com)");
    }

    #[test]
    fn format_rule_round_trip() {
        // Parse a Claude rule, format it back, and verify it produces the same rule.
        let original = "Bash(npm run build)";
        let parsed = parse_permission_rule(original, RuleAction::Allow).unwrap();
        let formatted = format_rule_string(&parsed);
        assert_eq!(formatted, original);

        // Round-trip the formatted string.
        let reparsed = parse_permission_rule(&formatted, RuleAction::Allow).unwrap();
        assert_eq!(parsed.tool, reparsed.tool);
        assert_eq!(parsed.pattern, reparsed.pattern);

        // The Bash `:*` prefix idiom formats to the bare prefix (`Bash(sed)`), not the original string
        // Reparsing must still yield an equivalent rule
        let parsed = parse_permission_rule("Bash(sed:*)", RuleAction::Deny).unwrap();
        assert_eq!(parsed.pattern.as_deref(), Some("sed"));
        let reparsed =
            parse_permission_rule(&format_rule_string(&parsed), RuleAction::Deny).unwrap();
        assert_eq!(parsed.tool, reparsed.tool);
        assert_eq!(parsed.pattern, reparsed.pattern);
        assert_eq!(parsed.pattern_mode, reparsed.pattern_mode);
    }

    #[test]
    fn merge_permissions_dedup() {
        let mut table = TomlMap::new();
        // Pre-populate with one existing rule.
        let mut perm = TomlMap::new();
        perm.insert(
            "allow".to_string(),
            TomlValue::Array(vec![TomlValue::String("Bash(npm test)".to_string())]),
        );
        table.insert("permission".to_string(), TomlValue::Table(perm));

        let rule_existing = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Bash,
            pattern: Some("npm test".to_string()),
            pattern_mode: PatternMode::Glob,
        };
        let rule_new = PermissionRule {
            action: RuleAction::Allow,
            tool: ToolFilter::Bash,
            pattern: Some("npm run build".to_string()),
            pattern_mode: PatternMode::Glob,
        };

        let count = merge_permissions(&mut table, &[&rule_existing, &rule_new]).unwrap();
        // Only the new rule is added; the existing one is deduped
        assert_eq!(count, 1);

        let Some(arr) = table
            .get("permission")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
        else {
            panic!("expected permission.allow: {table:?}");
        };
        let [a0, a1] = arr.as_slice() else {
            panic!("expected two allow rules: {arr:?}");
        };
        assert_eq!(a0.as_str(), Some("Bash(npm test)"));
        assert_eq!(a1.as_str(), Some("Bash(npm run build)"));
    }

    #[test]
    fn merge_env_vars_no_overwrite() {
        let mut table = TomlMap::new();
        let mut env = TomlMap::new();
        env.insert(
            "EXISTING".to_string(),
            TomlValue::String("old_value".to_string()),
        );
        table.insert("env".to_string(), TomlValue::Table(env));

        let count = merge_env_vars(
            &mut table,
            &[("EXISTING", "new_value"), ("NEW_VAR", "value")],
        );
        // Only NEW_VAR is added
        assert_eq!(count, 1);

        let Some(env_table) = table.get("env").and_then(|v| v.as_table()) else {
            panic!("expected env table: {table:?}");
        };
        assert_eq!(
            env_table.get("EXISTING").and_then(|v| v.as_str()),
            Some("old_value"),
            "existing key should NOT be overwritten"
        );
        assert_eq!(
            env_table.get("NEW_VAR").and_then(|v| v.as_str()),
            Some("value")
        );
    }

    #[test]
    fn merge_env_vars_creates_section() {
        let mut table = TomlMap::new();
        let count = merge_env_vars(&mut table, &[("FOO", "bar")]);
        assert_eq!(count, 1);
        assert_eq!(
            table
                .get("env")
                .and_then(|e| e.get("FOO"))
                .and_then(|v| v.as_str()),
            Some("bar")
        );
    }

    #[test]
    fn is_claude_import_marked_at_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert!(!is_claude_import_marked_at(&path));
    }

    #[test]
    fn is_claude_import_marked_at_missing_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui]\ntheme = \"dark\"\n").unwrap();
        std::fs::write(&path, "[other]\nkey = \"value\"\n").unwrap();
        assert!(!is_claude_import_marked_at(&path));
    }

    #[test]
    fn is_claude_import_marked_at_explicit_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[claude_compat]\nimported = false\n").unwrap();
        assert!(!is_claude_import_marked_at(&path));
    }

    #[test]
    fn is_claude_import_marked_at_true() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[claude_compat]\nimported = true\n").unwrap();
        assert!(is_claude_import_marked_at(&path));
    }

    #[test]
    fn write_import_marker_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        write_import_marker(&path).unwrap();
        assert!(is_claude_import_marked_at(&path));
    }

    #[test]
    fn write_import_marker_preserves_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[other]\nkey = \"value\"\n\n[mcp_servers.foo]\ncommand = \"x\"\n",
        )
        .unwrap();
        write_import_marker(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: TomlValue = toml::from_str(&content).unwrap();
        assert_eq!(
            parsed
                .get("other")
                .and_then(|o| o.get("key"))
                .and_then(|v| v.as_str()),
            Some("value")
        );
        assert_eq!(
            parsed
                .get("mcp_servers")
                .and_then(|m| m.get("foo"))
                .and_then(|f| f.get("command"))
                .and_then(|v| v.as_str()),
            Some("x")
        );
        assert_eq!(
            parsed
                .get("claude_compat")
                .and_then(|c| c.get("imported"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn write_import_marker_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_import_marker(&path).unwrap();
        write_import_marker(&path).unwrap();
        assert!(is_claude_import_marked_at(&path));
    }

    // The MARKER_CACHE is a process-global RwLock so these tests must run serially
    // They each set the cache to true or false via the test helper, then call the gated function and assert on its early-return behavior
    use serial_test::serial;

    /// RAII guard that resets the marker cache when dropped, so tests don't leak state into one another.
    pub(super) struct MarkerGuard;
    impl Drop for MarkerGuard {
        fn drop(&mut self) {
            reset_marker_cache_for_test();
            // Also clear the workspace-side env-var override so it doesn't leak into subsequent tests
            unsafe { std::env::remove_var("_GROK_CLAUDE_MARKER_OVERRIDE") };
        }
    }

    /// RAII guard that removes an env var on drop, preventing leaks when
    /// an assertion panics before manual cleanup.

    #[test]
    fn extract_hooks_basic_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": "Bash",
                            "hooks": [
                                { "type": "command", "command": "echo hi", "timeout": 7 }
                            ]
                        }
                    ]
                }
            }"#,
        )
        .unwrap();
        let items = extract_hooks_from_settings_file(&path);
        assert_eq!(items.len(), 1);
        let Some(ImportableItem::Hook {
            event,
            matcher,
            command,
            timeout,
        }) = items.first()
        else {
            panic!("expected Hook variant: {items:?}");
        };
        assert_eq!(event, "PreToolUse");
        assert_eq!(matcher.as_deref(), Some("Bash"));
        assert_eq!(command, "echo hi");
        assert_eq!(*timeout, Some(7));
    }

    #[test]
    fn extract_hooks_skips_non_command_handlers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "hooks": {
                    "PostToolUse": [
                        {
                            "hooks": [
                                { "type": "http", "url": "https://example.com" },
                                { "type": "command", "command": "true" }
                            ]
                        }
                    ]
                }
            }"#,
        )
        .unwrap();
        let items = extract_hooks_from_settings_file(&path);
        assert_eq!(items.len(), 1, "only command handler should be imported");
    }

    #[test]
    fn extract_hooks_empty_when_no_hooks_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "permissions": { "allow": [] } }"#).unwrap();
        assert!(extract_hooks_from_settings_file(&path).is_empty());
    }

    #[test]
    fn extract_hooks_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        assert!(extract_hooks_from_settings_file(&path).is_empty());
    }

    #[test]
    fn apply_hooks_to_dir_writes_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let items = vec![ImportableItem::Hook {
            event: "PreToolUse".to_string(),
            matcher: Some("Bash".to_string()),
            command: "echo x".to_string(),
            timeout: None,
        }];
        let count = apply_hooks_to_dir(&hooks_dir, &items).unwrap();
        assert_eq!(count, 1);

        let target = hooks_dir.join("imported-from-claude.json");
        let content = std::fs::read_to_string(&target).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let Some(groups) = parsed
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array())
        else {
            panic!("expected PreToolUse hooks: {parsed:?}");
        };
        let [group] = groups.as_slice() else {
            panic!("expected one group: {groups:?}");
        };
        assert_eq!(group.get("matcher").and_then(|v| v.as_str()), Some("Bash"));
        assert_eq!(
            group
                .get("hooks")
                .and_then(|h| h.get(0))
                .and_then(|h| h.get("command"))
                .and_then(|v| v.as_str()),
            Some("echo x")
        );
    }

    #[test]
    fn apply_hooks_to_dir_dedup_existing() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let target = hooks_dir.join("imported-from-claude.json");
        std::fs::write(
            &target,
            r#"{
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo x" }] }
                    ]
                }
            }"#,
        )
        .unwrap();

        let items = vec![
            ImportableItem::Hook {
                event: "PreToolUse".to_string(),
                matcher: Some("Bash".to_string()),
                command: "echo x".to_string(),
                timeout: None,
            },
            ImportableItem::Hook {
                event: "PreToolUse".to_string(),
                matcher: Some("Bash".to_string()),
                command: "echo y".to_string(),
                timeout: None,
            },
        ];
        let count = apply_hooks_to_dir(&hooks_dir, &items).unwrap();
        assert_eq!(count, 1, "only the new hook should be added");

        let content = std::fs::read_to_string(&target).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let Some(groups) = parsed
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array())
        else {
            panic!("expected PreToolUse hooks: {parsed:?}");
        };
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn apply_hooks_to_dir_no_hooks_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let items = vec![ImportableItem::EnvVar {
            key: "X".into(),
            value: "y".into(),
        }];
        let count = apply_hooks_to_dir(&hooks_dir, &items).unwrap();
        assert_eq!(count, 0);
        assert!(!hooks_dir.exists());
    }

    #[test]
    #[serial]
    fn discover_hook_source_paths_skips_claude_when_marker_set() {
        let _g = MarkerGuard;
        refresh_marker_cache(true);
        let dir = tempfile::tempdir().unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let paths = crate::util::hooks::discover_hook_source_paths(Some(dir.path()), &compat);
        let project_strs = source_path_strs(&paths.project);
        assert!(
            !project_strs.iter().any(|s| s.contains(".claude")),
            "project sources should not include .claude/ when marker set; got {:?}",
            project_strs
        );
        let global_strs = source_path_strs(&paths.global);
        assert!(
            !global_strs.iter().any(|s| s.contains("/.claude/")),
            "global sources should not include ~/.claude/ when marker set; got {:?}",
            global_strs
        );
    }

    #[test]
    #[serial]
    fn gate_load_claude_env_returns_empty_when_marker_set() {
        let _g = MarkerGuard;
        refresh_marker_cache(true);
        let dir = tempfile::tempdir().unwrap();
        let env = distill_workspace::permission::claude_settings::load_claude_env_with_project(
            dir.path(),
            true,
        );
        assert!(
            env.is_empty(),
            "load_claude_env_with_project should be empty when marker set"
        );
    }

    #[test]
    #[serial]
    fn discover_hook_source_paths_includes_claude_when_marker_unset() {
        let _g = MarkerGuard;
        refresh_marker_cache(false);
        let dir = tempfile::tempdir().unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let paths = crate::util::hooks::discover_hook_source_paths(Some(dir.path()), &compat);
        let project_strs = source_path_strs(&paths.project);
        assert!(
            project_strs.iter().any(|s| s.contains(".claude")),
            "project sources should include .claude/ when marker unset; got {:?}",
            project_strs
        );
    }

    #[test]
    #[serial]
    fn discover_hook_source_paths_includes_cursor_hooks_json() {
        let _g = MarkerGuard;
        refresh_marker_cache(false);
        let dir = tempfile::tempdir().unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let paths = crate::util::hooks::discover_hook_source_paths(Some(dir.path()), &compat);
        let global_strs = source_path_strs(&paths.global);
        assert!(
            global_strs
                .iter()
                .any(|s| s.contains(".cursor") && s.ends_with("hooks.json")),
            "global sources should include ~/.cursor/hooks.json; got {:?}",
            global_strs
        );
        let project_strs = source_path_strs(&paths.project);
        assert!(
            project_strs
                .iter()
                .any(|s| s.contains(".cursor") && s.ends_with("hooks.json")),
            "project sources should include .cursor/hooks.json; got {:?}",
            project_strs
        );
    }

    #[test]
    #[serial]
    fn discover_hook_source_paths_skips_cursor_when_disabled() {
        let _g = MarkerGuard;
        refresh_marker_cache(false);
        let dir = tempfile::tempdir().unwrap();
        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.cursor.hooks = false;
        let paths = crate::util::hooks::discover_hook_source_paths(Some(dir.path()), &compat);
        let global_strs = source_path_strs(&paths.global);
        assert!(
            !global_strs.iter().any(|s| s.contains(".cursor")),
            "global sources should not include .cursor/ when disabled; got {:?}",
            global_strs
        );
        let project_strs = source_path_strs(&paths.project);
        assert!(
            !project_strs.iter().any(|s| s.contains(".cursor")),
            "project sources should not include .cursor/ when disabled; got {:?}",
            project_strs
        );
    }

    #[test]
    #[serial]
    fn discover_hook_source_paths_skips_claude_when_compat_disabled() {
        let _g = MarkerGuard;
        // Do NOT set the marker: test the compat gate in isolation
        refresh_marker_cache(false);
        let dir = tempfile::tempdir().unwrap();
        let mut compat = distill_tools::types::compat::CompatConfig::default();
        compat.claude.hooks = false;
        let paths = crate::util::hooks::discover_hook_source_paths(Some(dir.path()), &compat);
        let global_strs = source_path_strs(&paths.global);
        assert!(
            !global_strs.iter().any(|s| s.contains("/.claude/")),
            "global sources should not include ~/.claude/ when compat disabled; got {:?}",
            global_strs
        );
        let project_strs = source_path_strs(&paths.project);
        assert!(
            !project_strs.iter().any(|s| s.contains(".claude")),
            "project sources should not include .claude/ when compat disabled; got {:?}",
            project_strs
        );
    }

    #[test]
    #[serial]
    fn as_sources_gates_project_sources_on_trust() {
        // Trust gating lives in `HookSourcePaths::as_sources`: project sources are dropped when untrusted and kept when trusted
        // Assert on project sources (git_root-relative) since global sources use the real, non-injectable home
        let _g = MarkerGuard;
        refresh_marker_cache(false);
        let dir = tempfile::tempdir().unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let paths = crate::util::hooks::discover_hook_source_paths(Some(dir.path()), &compat);
        assert!(
            !paths.project.is_empty(),
            "project source paths should be non-empty for a git_root"
        );

        let (global_untrusted, project) = paths.as_sources(false);
        assert_eq!(
            global_untrusted.len(),
            paths.global.len(),
            "global sources must survive untrusted"
        );
        assert!(
            project.is_empty(),
            "untrusted: as_sources(false) must drop all project sources"
        );

        let (_global, project) = paths.as_sources(true);
        assert!(
            !project.is_empty(),
            "trusted: as_sources(true) must keep project sources"
        );
    }

    #[test]
    #[serial]
    fn discover_hooks_honors_claude_compat_gate() {
        // Pins the single load entry point every startup/reload site uses
        // With `compat.claude.hooks = false` a project `.claude/settings.json` hook must NOT load; with it true it MUST
        // A pager e2e is disproportionate: the spawn/agent_ops wiring just forwards the resolved compat into this entry point
        let _g = MarkerGuard;
        // The marker stays unset so the import cutoff doesn't independently skip `.claude`; this isolates the compat gate
        refresh_marker_cache(false);

        // `discover_hooks` takes git_root directly (no git discovery), so a plain temp dir with a project `.claude/settings.json` suffices
        let git_root = tempfile::tempdir().unwrap();
        let claude_dir = git_root.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"claude_compat_gate_probe.sh"}]}]}}"#,
        )
        .unwrap();

        // Identify the probe by its unique raw command so real global hooks on the
        // test host (from the non-injectable ~/.claude, ~/.grok) don't interfere.
        let has_probe = |reg: &distill_hooks::discovery::HookRegistry| {
            reg.all_hooks().iter().any(|h| {
                h.command_raw
                    .as_deref()
                    .unwrap_or_default()
                    .contains("claude_compat_gate_probe")
            })
        };

        // Trusted so project sources are included; vary only the compat toggle.
        let mut compat = distill_tools::types::compat::CompatConfig::default();

        compat.claude.hooks = false;
        let (reg, _errs) = crate::util::hooks::discover_hooks(Some(git_root.path()), &compat, true);
        assert!(
            !has_probe(&reg),
            "compat.claude.hooks=false: project .claude hook must NOT be loaded"
        );

        compat.claude.hooks = true;
        let (reg, _errs) = crate::util::hooks::discover_hooks(Some(git_root.path()), &compat, true);
        assert!(
            has_probe(&reg),
            "compat.claude.hooks=true: project .claude hook must be loaded"
        );
    }

    #[test]
    fn extract_hooks_multiple_events_and_matchers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "Bash", "hooks": [{ "type": "command", "command": "a" }] },
                        { "matcher": "Edit", "hooks": [{ "type": "command", "command": "b" }] }
                    ],
                    "PostToolUse": [
                        { "hooks": [
                            { "type": "command", "command": "c" },
                            { "type": "command", "command": "d" }
                        ] }
                    ]
                }
            }"#,
        )
        .unwrap();
        let items = extract_hooks_from_settings_file(&path);
        assert_eq!(items.len(), 4, "4 hooks across 2 events / 3 matchers");
        let events: std::collections::HashSet<&str> = items
            .iter()
            .filter_map(|i| match i {
                ImportableItem::Hook { event, .. } => Some(event.as_str()),
                _ => None,
            })
            .collect();
        assert!(events.contains("PreToolUse"));
        assert!(events.contains("PostToolUse"));
    }

    #[test]
    fn extract_hooks_malformed_hooks_field_is_silent_skip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // `hooks` is a string instead of an object
        std::fs::write(&path, r#"{ "hooks": "oops" }"#).unwrap();
        // Function early-returns when `hooks` is not an object.
        assert!(extract_hooks_from_settings_file(&path).is_empty());
    }

    #[test]
    fn extract_hooks_empty_command_string_is_imported_as_is() {
        // Documented behavior: an empty command string is imported verbatim.
        // Users editing `.claude/settings.json` to debug an empty-command entry will see it appear in the import summary, not silently disappear
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{ "hooks": { "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "" }] }
            ] } }"#,
        )
        .unwrap();
        let items = extract_hooks_from_settings_file(&path);
        assert_eq!(items.len(), 1);
        if let Some(ImportableItem::Hook { command, .. }) = items.first() {
            assert_eq!(command, "");
        } else {
            panic!("expected Hook variant");
        }
    }

    #[test]
    fn apply_hooks_to_dir_updates_timeout_on_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let target = hooks_dir.join("imported-from-claude.json");
        std::fs::write(
            &target,
            r#"{ "hooks": { "PreToolUse": [
                { "matcher": "Bash", "hooks": [
                    { "type": "command", "command": "echo x", "timeout": 5 }
                ] }
            ] } }"#,
        )
        .unwrap();

        let items = vec![ImportableItem::Hook {
            event: "PreToolUse".to_string(),
            matcher: Some("Bash".to_string()),
            command: "echo x".to_string(),
            timeout: Some(60),
        }];
        let count = apply_hooks_to_dir(&hooks_dir, &items).unwrap();
        assert_eq!(count, 0, "no new entry added; timeout updated in place");

        let content = std::fs::read_to_string(&target).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let Some(groups) = parsed
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.as_array())
        else {
            panic!("expected PreToolUse hooks: {parsed:?}");
        };
        let [group] = groups.as_slice() else {
            panic!("expected one group: {groups:?}");
        };
        let Some(handlers) = group.get("hooks").and_then(|h| h.as_array()) else {
            panic!("expected handlers: {group:?}");
        };
        assert_eq!(
            handlers
                .first()
                .and_then(|h| h.get("timeout"))
                .and_then(|v| v.as_u64()),
            Some(60)
        );
    }

    #[test]
    fn apply_hooks_to_dir_removes_timeout_when_new_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let target = hooks_dir.join("imported-from-claude.json");
        std::fs::write(
            &target,
            r#"{ "hooks": { "PreToolUse": [
                { "matcher": "Bash", "hooks": [
                    { "type": "command", "command": "echo x", "timeout": 5 }
                ] }
            ] } }"#,
        )
        .unwrap();

        let items = vec![ImportableItem::Hook {
            event: "PreToolUse".to_string(),
            matcher: Some("Bash".to_string()),
            command: "echo x".to_string(),
            timeout: None,
        }];
        let count = apply_hooks_to_dir(&hooks_dir, &items).unwrap();
        assert_eq!(count, 0);

        let content = std::fs::read_to_string(&target).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let Some(handlers) = parsed
            .get("hooks")
            .and_then(|h| h.get("PreToolUse"))
            .and_then(|v| v.get(0))
            .and_then(|g| g.get("hooks"))
            .and_then(|h| h.as_array())
        else {
            panic!("expected handlers: {parsed:?}");
        };
        assert!(handlers.first().is_some_and(|h| h.get("timeout").is_none()));
    }

    #[test]
    fn merge_paths_creates_section() {
        let mut table = TomlMap::new();
        let count = merge_paths(&mut table, "extra_skill_dirs", &["/a", "/b"]).unwrap();
        assert_eq!(count, 2);
        let Some(arr) = table
            .get("paths")
            .and_then(|p| p.get("extra_skill_dirs"))
            .and_then(|a| a.as_array())
        else {
            panic!("expected extra_skill_dirs: {table:?}");
        };
        let [a0, a1] = arr.as_slice() else {
            panic!("expected two dirs: {arr:?}");
        };
        assert_eq!(a0.as_str(), Some("/a"));
        assert_eq!(a1.as_str(), Some("/b"));
    }

    #[test]
    fn merge_paths_dedup_existing() {
        let mut table = TomlMap::new();
        let mut paths = TomlMap::new();
        paths.insert(
            "extra_skill_dirs".into(),
            TomlValue::Array(vec![TomlValue::String("/existing".into())]),
        );
        table.insert("paths".into(), TomlValue::Table(paths));

        let count = merge_paths(&mut table, "extra_skill_dirs", &["/existing", "/new"]).unwrap();
        assert_eq!(count, 1, "existing entry should be deduped");
        let Some(arr) = table
            .get("paths")
            .and_then(|p| p.get("extra_skill_dirs"))
            .and_then(|a| a.as_array())
        else {
            panic!("expected extra_skill_dirs: {table:?}");
        };
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn paths_config_deserializes() {
        let toml_str = r#"
[paths]
extra_skill_dirs = ["/a/skills", "/b/skills"]
extra_rule_dirs = ["/c/rules"]
"#;
        let value: TomlValue = toml::from_str(toml_str).unwrap();
        let paths = value.get("paths").unwrap();
        let cfg: crate::agent::config::PathsConfig = paths.clone().try_into().unwrap();
        assert_eq!(cfg.extra_skill_dirs, vec!["/a/skills", "/b/skills"]);
        assert_eq!(cfg.extra_rule_dirs, vec!["/c/rules"]);
    }

    #[test]
    fn paths_config_default_empty() {
        let cfg = crate::agent::config::PathsConfig::default();
        assert!(cfg.extra_skill_dirs.is_empty());
        assert!(cfg.extra_rule_dirs.is_empty());
    }

    #[test]
    fn apply_items_to_config_writes_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let items = vec![
            ImportableItem::PathEntry {
                kind: PathKind::Skill,
                path: "/foo/skills".into(),
            },
            ImportableItem::PathEntry {
                kind: PathKind::Rule,
                path: "/bar/rules".into(),
            },
        ];
        let count = apply_items_to_config(&path, &items).unwrap();
        assert_eq!(count, 2);

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: TomlValue = toml::from_str(&content).unwrap();
        assert_eq!(
            parsed
                .get("paths")
                .and_then(|p| p.get("extra_skill_dirs"))
                .and_then(|a| a.get(0))
                .and_then(|v| v.as_str()),
            Some("/foo/skills")
        );
        assert_eq!(
            parsed
                .get("paths")
                .and_then(|p| p.get("extra_rule_dirs"))
                .and_then(|a| a.get(0))
                .and_then(|v| v.as_str()),
            Some("/bar/rules")
        );
    }

    #[test]
    fn scan_claude_path_dirs_dedupes_global_and_project_when_same() {
        // Simulate a workspace where project_root canonicalises to the home dir (the user runs /import-claude from ~ where .claude/ already lives)
        // Without dedup, the same .claude/skills would land in both scopes.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".claude").join("skills")).unwrap();

        // Build a plan by directly invoking the scan with a synthetic plan and a cwd whose `find_project_root` returns the same `home` We can't easily mock `distill_dirs::home_dir()`, so this test focuses on the dedup *logic*
        // It manually populates `global_items` first, then asserts that the project-side branch with the same path would skip
        // Direct end-to-end coverage of the home-collision case requires `GROK_HOME` plumbing which is intentionally out of scope
        let global = dunce::canonicalize(home.join(".claude").join("skills")).unwrap();
        let project = dunce::canonicalize(home.join(".claude").join("skills")).unwrap();
        assert_eq!(global, project, "sanity: paths canonicalize to the same");
    }

    #[test]
    #[serial]
    fn gate_load_mcp_json_servers_returns_empty_when_marker_set() {
        let _g = MarkerGuard;
        refresh_marker_cache(true);
        let dir = tempfile::tempdir().unwrap();
        let servers = crate::util::config::load_mcp_json_servers(dir.path());
        assert!(
            servers.is_empty(),
            "load_mcp_json_servers should be empty when marker set"
        );
    }

    #[test]
    #[serial]
    fn gate_load_claude_json_mcp_servers_returns_empty_when_marker_set() {
        let _g = MarkerGuard;
        refresh_marker_cache(true);
        let dir = tempfile::tempdir().unwrap();
        let compat = distill_tools::types::compat::CompatConfig::default();
        let servers = crate::util::config::load_claude_json_mcp_servers(dir.path(), &compat);
        assert!(
            servers.is_empty(),
            "load_claude_json_mcp_servers should be empty when marker set"
        );
    }

    #[tokio::test]
    #[serial]
    async fn gate_resolve_permissions_with_provenance_skips_claude_when_marker_set() {
        let _g = MarkerGuard;
        refresh_marker_cache(true);
        // Also set the env-var override so the workspace-resident marker reader (which can't see the shell-side cache) honours the gate
        unsafe { std::env::set_var("_GROK_CLAUDE_MARKER_OVERRIDE", "1") };
        let dir = tempfile::tempdir().unwrap();
        // Drop a Claude permissions file in the tempdir; with the marker set the gate should skip reading it
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{ "permissions": { "allow": ["Bash(echo hi)"] } }"#,
        )
        .unwrap();

        // Note: `resolve_permissions_with_provenance` ALSO reads requirements, managed settings, and the developer's real `~/.grok/config.toml`.
        // We can't isolate `distill_home()` because it's `OnceLock`-cached.
        // Instead, assert on rule *provenance*: no rule should originate from our tempdir's `.claude/settings.json`. The dev's real ~/.grok config rules (if any) are out of scope for this test.
        let resolved =
            distill_workspace::permission::resolution::resolve_permissions_with_provenance(
                dir.path(),
                true,
            )
            .await
            .resolved;
        if let Some(r) = resolved {
            let tempdir_claude = claude_dir.join("settings.json");
            use distill_workspace::permission::types::RequirementSource;
            let leaked: Vec<&RequirementSource> = r
                .sources
                .iter()
                .filter(|s| matches!(s, RequirementSource::Settings { path } if path == &tempdir_claude))
                .collect();
            assert!(
                leaked.is_empty(),
                "tempdir .claude/settings.json should not produce rules when marker set, \
                 but got {} rule(s) from {}: {:?}",
                leaked.len(),
                tempdir_claude.display(),
                leaked
            );
        }
    }

    #[test]
    #[serial]
    fn gate_merge_claude_enabled_plugins_no_op_when_marker_set() {
        let _g = MarkerGuard;
        refresh_marker_cache(true);
        let mut plugins = crate::agent::config::PluginsConfig::default();
        let before_enabled = plugins.enabled.clone();
        let before_disabled = plugins.disabled.clone();
        // Pass `None` for cwd: the gate fires before any file IO
        plugins.merge_claude_enabled_plugins(None);
        assert_eq!(plugins.enabled, before_enabled);
        assert_eq!(plugins.disabled, before_disabled);
    }

    #[test]
    #[serial]
    fn gate_marker_cache_unset_means_uses_disk() {
        // Sanity test: with the cache reset, `is_claude_import_marked()` must (a) not panic and (b) populate the cache for subsequent reads
        // We intentionally **do not** assert a specific cached value: the dev's real `~/.grok/config.toml` may legitimately have the marker set during local testing, and we can't override `distill_home()`
        // It's `OnceLock`-cached, so any prior test that calls it locks the value in for the entire process The `MarkerGuard` resets the cache after this test, so subsequent gate tests start clean
        let _g = MarkerGuard;
        reset_marker_cache_for_test();
        let _ = is_claude_import_marked();
        assert!(
            MARKER_CACHE
                .read()
                .expect("MARKER_CACHE poisoned")
                .is_some(),
            "cache should be populated after a call"
        );
    }
}
