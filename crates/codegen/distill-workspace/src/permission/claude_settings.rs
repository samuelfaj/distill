// Modified for Distill by Samuel Fajreldines, 2026.
//! Reads and parses `.claude/settings.json` (vendor settings interop).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, warn};

use crate::permission::rules::parse_permission_rule;
use crate::permission::types::{PermissionConfig, RuleAction};

// ═════════════════════════════════════════════════════════════════════════════
// Settings Types (Claude JSON subset)
// ═════════════════════════════════════════════════════════════════════════════

/// Subset of `.claude/settings.json` we care about.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSettings {
    #[serde(default)]
    pub permissions: Option<ParsedPermissions>,

    /// Raw `defaultMode` string when present (canonical under `permissions`, or grok-only root legacy).
    /// Recognized values: `acceptEdits`, `bypassPermissions`, `default`, `plan`, `dontAsk`, `auto`.
    #[serde(default)]
    pub default_mode: Option<String>,

    /// Parsed but not acted on yet.
    #[serde(default)]
    pub additional_directories: Option<Vec<String>>,

    /// Environment variables applied to every session.
    /// Keys and values are strings; non-string values are coerced or skipped.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

/// Parsed `permissions` object from Claude settings.
#[derive(Debug, Default, Deserialize)]
pub struct ParsedPermissions {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
}

impl ParsedPermissions {
    /// Unsupported or malformed entries are skipped with warnings.
    pub fn into_permission_config(self) -> (PermissionConfig, Vec<String>) {
        let mut rules = Vec::new();
        let mut warnings = Vec::new();

        for (action, entries, label) in [
            (RuleAction::Allow, self.allow, "allow"),
            (RuleAction::Deny, self.deny, "deny"),
            (RuleAction::Ask, self.ask, "ask"),
        ] {
            for rule_str in entries {
                match parse_permission_rule(&rule_str, action) {
                    Ok(rule) => rules.push(rule),
                    Err(e) => warnings.push(format!("permissions.{label}: {rule_str} -- {e}")),
                }
            }
        }

        (PermissionConfig::new(rules), warnings)
    }
}

/// Returns `None` only when the file is missing, unreadable, or unparseable JSON.
/// A `Some(ClaudeSettings)` may lack the `permissions` key, so callers still see `defaultMode` and `additionalDirectories`.
/// Non-string array entries are skipped with warnings, and the canonical `permissions.*` keys win over the grok-legacy root keys.
pub fn load_claude_settings(path: &Path) -> Option<ClaudeSettings> {
    // Opening a FIFO for read blocks until a writer appears; this runs on the session actor, so refuse non-regular files up front
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            tracing::warn!(?path, "refusing to read non-regular settings file");
            return None;
        }
        Err(_) => return None,
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return None,
    };

    // Parse as generic JSON value for tolerant handling
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return None,
    };

    // Extract permissions tolerantly if present (with warnings for non-strings)
    let permissions = value.get("permissions").and_then(|p| {
        let (allow, allow_warnings) = extract_string_array(p.get("allow"));
        let (deny, deny_warnings) = extract_string_array(p.get("deny"));
        let (ask, ask_warnings) = extract_string_array(p.get("ask"));

        for w in allow_warnings
            .iter()
            .chain(deny_warnings.iter())
            .chain(ask_warnings.iter())
        {
            tracing::warn!(path = %path.display(), "{}", w);
        }

        if allow.is_empty() && deny.is_empty() && ask.is_empty() {
            None
        } else {
            Some(ParsedPermissions { allow, deny, ask })
        }
    });

    let default_mode = extract_default_mode(&value, path);

    let additional_directories = extract_additional_directories(&value, path);

    let env = extract_string_map(value.get("env"), path);

    Some(ClaudeSettings {
        permissions,
        default_mode,
        additional_directories,
        env,
    })
}

/// Canonical key is `permissions.defaultMode`.
/// Root `defaultMode` is grok-only back-compat; use it only when the nested key is absent, never when nested is present but not a string.
pub(crate) fn extract_default_mode(value: &serde_json::Value, path: &Path) -> Option<String> {
    if let Some(perms) = value.get("permissions")
        && let Some(dm) = perms.get("defaultMode")
    {
        return match dm.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                warn!(
                    path = %path.display(),
                    actual_type = %dm.type_of(),
                    "permissions.defaultMode: expected string; not falling back to root defaultMode"
                );
                None
            }
        };
    }

    // Nested key absent, fall back to the optional grok legacy root
    match value.get("defaultMode") {
        Some(dm) => match dm.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                warn!(
                    path = %path.display(),
                    actual_type = %dm.type_of(),
                    "root defaultMode (grok legacy): expected string, ignoring"
                );
                None
            }
        },
        None => None,
    }
}

/// Claude-canonical key is `permissions.additionalDirectories`; root is legacy/compat.
/// Nested wins when both are present.
fn extract_additional_directories(value: &serde_json::Value, path: &Path) -> Option<Vec<String>> {
    // Mirror `extract_default_mode`: prefer the Claude-canonical nested key
    // A nested key of the wrong type does *not* resurrect the grok-legacy root value
    let arr = if let Some(nested) = value
        .get("permissions")
        .and_then(|p| p.get("additionalDirectories"))
    {
        match nested.as_array() {
            Some(arr) => arr,
            None => {
                warn!(
                    path = %path.display(),
                    actual_type = %nested.type_of(),
                    "permissions.additionalDirectories: expected array; not falling back to root additionalDirectories"
                );
                return None;
            }
        }
    } else {
        value
            .get("additionalDirectories")
            .and_then(|v| v.as_array())?
    };

    let mut result = Vec::new();
    for (i, v) in arr.iter().enumerate() {
        match v.as_str() {
            Some(s) => result.push(s.to_string()),
            None => tracing::warn!(
                path = %path.display(),
                index = i,
                actual_type = %v.type_of(),
                "additionalDirectories: expected string, skipping"
            ),
        }
    }
    Some(result)
}

/// Extract a string array from a JSON value; non-string entries are skipped and described in the returned warnings.
fn extract_string_array(value: Option<&serde_json::Value>) -> (Vec<String>, Vec<String>) {
    match value {
        Some(serde_json::Value::Array(arr)) => {
            let mut strings = Vec::new();
            let mut warnings = Vec::new();
            for (i, v) in arr.iter().enumerate() {
                match v.as_str() {
                    Some(s) => strings.push(s.to_string()),
                    None => warnings.push(format!(
                        "permissions array index {}: expected string, got {}",
                        i,
                        v.type_of()
                    )),
                }
            }
            (strings, warnings)
        }
        Some(other) => {
            let warnings = vec![format!(
                "permissions field: expected array, got {}",
                other.type_of()
            )];
            (Vec::new(), warnings)
        }
        None => (Vec::new(), Vec::new()),
    }
}

/// Numbers and booleans are coerced to their string form; null, array, and object values are skipped with warnings.
/// Nulls are not coerced to the literal `"null"` because an env var set to `"null"` is more likely a user mistake.
fn extract_string_map(
    value: Option<&serde_json::Value>,
    path: &Path,
) -> Option<HashMap<String, String>> {
    let obj = match value {
        Some(serde_json::Value::Object(map)) => map,
        Some(other) => {
            tracing::warn!(
                path = %path.display(),
                actual_type = %other.type_of(),
                "env: expected object, skipping"
            );
            return None;
        }
        None => return None,
    };

    let mut result = HashMap::new();
    for (key, val) in obj {
        match val {
            serde_json::Value::String(s) => {
                result.insert(key.clone(), s.clone());
            }
            serde_json::Value::Number(n) => {
                result.insert(key.clone(), n.to_string());
            }
            serde_json::Value::Bool(b) => {
                result.insert(key.clone(), b.to_string());
            }
            other => {
                tracing::warn!(
                    path = %path.display(),
                    key = %key,
                    actual_type = %other.type_of(),
                    "env: expected string value, skipping"
                );
            }
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Type name of a JSON value, used in warnings.
trait JsonTypeName {
    fn type_of(&self) -> &'static str;
}
impl JsonTypeName for serde_json::Value {
    fn type_of(&self) -> &'static str {
        match self {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Discovery
// ═════════════════════════════════════════════════════════════════════════════

// TODO: settings discovery is local to this module; extract a shared helper if more than permissions consume it.

/// Discover `.claude/settings.json` and `.claude/settings.local.json`, most-specific first: project cwd down to repo root, then `~/.claude`.
/// Within a directory, `settings.local.json` precedes `settings.json`.
pub fn has_claude_compat(cwd: &Path) -> bool {
    find_claude_settings_paths(cwd).iter().any(|p| p.exists())
}

/// Permission rules from all files are merged (later / more specific sources win for conflicts).
/// `defaultMode` uses scope precedence: the most specific file that sets it wins.
pub fn find_claude_settings_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = global_claude_settings_paths();

    // Project paths (higher priority; closer to cwd wins)
    let project_paths = collect_project_claude_paths(cwd);
    paths.splice(0..0, project_paths);

    paths
}

/// User-tier `~/.claude` paths, highest-priority-first, so an untrusted folder can load only this tier.
/// `distill_dirs::home_dir()` matches Node `os.homedir()` so these paths test as global in the import scanner.
fn global_claude_settings_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = distill_dirs::home_dir() {
        let global = home.join(".claude");
        paths.push(global.join("settings.local.json"));
        paths.push(global.join("settings.json"));
    }
    paths
}

/// Settings files under the folder-trust gate: full tree when trusted, user-tier `~/.claude` only when not.
/// Env injection and permission resolution both go through here so they cannot drift on what an untrusted clone may contribute.
pub(crate) fn claude_settings_paths_for_trust(cwd: &Path, project_trusted: bool) -> Vec<PathBuf> {
    if project_trusted {
        find_claude_settings_paths(cwd)
    } else {
        global_claude_settings_paths()
    }
}

/// Whether a project-tree `.claude/settings.json` / `settings.local.json` exists anywhere on the walk from `cwd` up to the repo root.
/// The folder-trust detector shares that walk ([`collect_project_claude_paths`]) with the env/permission loaders, so detection can never drift.
/// A settings file in a SUBDIR, whose `env` is injected into every spawned subprocess, must flip the folder untrusted, not just one at the git root.
/// Presence is type-agnostic to match the hook loader: a directory at the settings path must gate too.
pub fn project_claude_settings_present(cwd: &Path) -> bool {
    collect_project_claude_paths(cwd)
        .iter()
        .any(|p| crate::util::path_present_or_uncertain(p))
}

/// Collect `.claude` settings from cwd up to repo root.
/// Root is `.git` existence, not `git2` validity, so a bare or empty `.git` still bounds the walk; loader and trust detector share this so they cannot drift.
fn collect_project_claude_paths(cwd: &Path) -> Vec<PathBuf> {
    // When `$HOME` is itself a git repo, drop that root so `~/.claude` is not treated as project-tier for every cwd under home
    // Fall back to cwd; this is the shared choke point for `project_claude_settings_present` and `find_claude_settings_paths`
    let repo_root = find_repo_root(cwd)
        .filter(|root| !crate::trust::is_home_dir(root))
        .unwrap_or_else(|| cwd.to_path_buf());

    // Walk from cwd up to repo_root, collecting .claude paths (cwd-first priority).
    let mut paths = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        let claude_dir = current.join(".claude");
        paths.push(claude_dir.join("settings.local.json"));
        paths.push(claude_dir.join("settings.json"));

        if current == repo_root {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    paths
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return None,
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Environment Variables
// ═════════════════════════════════════════════════════════════════════════════

/// Merge Claude settings `env`, later keys overriding earlier (cwd highest, `settings.local.json` over `settings.json`).
/// Repo-tree `env` is injected into every spawned subprocess, so it is dropped unless `project_trusted`; user `~/.claude` env is always loaded.
pub fn load_claude_env_with_project(cwd: &Path, project_trusted: bool) -> HashMap<String, String> {
    // Phase 2 cutoff: if the user has imported, skip reading .claude/ at runtime.
    if is_claude_import_marked_with_log("load_claude_env_with_project") {
        return HashMap::new();
    }

    // Untrusted folder: load ONLY the user-tier `~/.claude` env, dropping the repo-tree (project) contribution
    let paths = claude_settings_paths_for_trust(cwd, project_trusted);
    let mut merged = HashMap::new();

    // Paths are ordered highest-priority-first
    // Process in reverse so that higher-priority values overwrite lower-priority ones via `extend`
    for path in paths.iter().rev() {
        if let Some(settings) = load_claude_settings(path)
            && let Some(env) = settings.env
        {
            debug!(
                path = %path.display(),
                count = env.len(),
                "Loaded env from Claude settings"
            );
            merged.extend(env);
        }
    }

    merged
}

// Phase 2 cutoff marker. Reader is local because gate consumers cannot depend on shell (cycle); caching omitted until this is a hotspot.

/// True when the user marked Claude settings imported (`[claude_compat].imported` in config.toml, or the test override).
/// Public so callers that mirror this gate elsewhere use the same check.
pub fn is_claude_import_marked() -> bool {
    // Test escape hatch: shell tests call `refresh_marker_cache(true)`, which lives in distill-shell (inaccessible from here at runtime)
    // They also set this env var so the gate in this crate honours the override without a cross-crate dependency
    if std::env::var("_GROK_CLAUDE_MARKER_OVERRIDE").as_deref() == Ok("1") {
        return true;
    }
    let Some(config_path) = distill_config::user_distill_home().map(|g| g.join("config.toml")) else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(&config_path) else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&contents) else {
        return false;
    };
    value
        .get("claude_compat")
        .and_then(|v| v.get("imported"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Logs a single info line the first time the gate is hit (per process) so we can confirm the cutoff is taking effect without flooding logs.
pub(crate) fn is_claude_import_marked_with_log(gate_name: &'static str) -> bool {
    use std::sync::OnceLock;
    static LOGGED: OnceLock<()> = OnceLock::new();

    let marked = is_claude_import_marked();
    if marked {
        LOGGED.get_or_init(|| {
            tracing::info!(
                first_gate = gate_name,
                "Claude compat disabled (marker set in config.toml)"
            );
        });
    }
    marked
}
