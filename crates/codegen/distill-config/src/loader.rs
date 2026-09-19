// Modified for Distill by Samuel Fajreldines, 2026.
//! TOML loading, layered merging, and `$VAR` expansion.
//!
//! The merged result is the **default** config; requirements layers sit on top via [`crate::validation`].

use std::path::Path;

use crate::paths::{system_config_dir, user_distill_home};
use crate::version_overrides::{self, apply_version_overrides};
use distill_dirs::resolve_distill_home;

/// Read and parse a TOML file WITHOUT `$VAR` expansion (empty table if absent).
/// Shared core of [`load_toml_file`] and the hook-layer read.
fn read_toml_file(path: &Path) -> std::io::Result<toml::Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(toml::Value::Table(toml::map::Map::new())),
        Ok(s) => match toml::from_str::<toml::Value>(&s) {
            Ok(v) => Ok(v),
            Err(e) => {
                // The detail is built from the span, never from Display: Display echoes the offending source line, which may carry a secret
                // Safe to log and to return to a client
                let detail = toml_error_detail(&s, &e);
                tracing::error!(file = %path.display(), "config toml has syntax errors: {detail}");
                Err(std::io::Error::other(detail))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::map::Map::new()))
        }
        Err(e) => {
            tracing::error!(file = %path.display(), "config file unreadable: {e}");
            Err(e)
        }
    }
}

/// Load and parse a TOML file, expanding `$VAR` references. Empty table if absent.
pub fn load_toml_file(path: &Path) -> std::io::Result<toml::Value> {
    let mut v = read_toml_file(path)?;
    expand_env_vars_in_toml(&mut v);
    Ok(v)
}

/// A snippet-free description of a TOML parse error: `"TOML parse error at line L, column C: <what>"` (or just the message when there's no span).
/// Never includes the offending source line (`Display` echoes it and it may carry a secret), so this is safe to log or return to a client.
/// Shared with the trace `config_files` artifact so the redaction rule lives in one place.
pub fn toml_error_detail(src: &str, e: &toml::de::Error) -> String {
    match e.span() {
        Some(span) => {
            let (line, col) = line_col(src, span.start);
            format!(
                "TOML parse error at line {line}, column {col}: {}",
                e.message()
            )
        }
        None => e.message().to_owned(),
    }
}

/// 1-based (line, column) of a byte offset within `src`.
fn line_col(src: &str, byte: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in src.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// [`load_toml_file`] plus that layer's `[[version_overrides]]`.
/// Use for grok config files; use [`load_toml_file`] directly for unrelated TOML.
pub fn load_config_file(path: &Path) -> std::io::Result<toml::Value> {
    let mut v = load_toml_file(path)?;
    apply_version_overrides_with_registered(&mut v)?;
    Ok(v)
}

pub fn load_from_disk() -> std::io::Result<toml::Value> {
    // Live `$GROK_HOME`: `user_distill_home()` / `distill_home()` are OnceLock and miss
    // EnvGuard/tests (same reason user `config.toml` persist resolves live). A
    // stale cache would read a different file than the last settings write.
    load_user_config_layer(resolve_distill_home().as_deref(), USER_CONFIG_FILENAME)
}

/// User config filename (`$GROK_HOME/config.toml`), shared by the loaders here.
pub const USER_CONFIG_FILENAME: &str = "config.toml";

/// Managed config filename, shared by the loaders in this module.
pub const MANAGED_CONFIG_FILENAME: &str = "managed_config.toml";

/// Requirements (cloud-cache) filename, synced from the server alongside the managed config.
pub const REQUIREMENTS_FILENAME: &str = "requirements.toml";

/// Unsigned folder-trust store (`$GROK_HOME/trusted_folders.toml`).
pub const TRUSTED_FOLDERS_FILENAME: &str = "trusted_folders.toml";

/// User-global sandbox profile definitions (`$GROK_HOME/sandbox.toml`).
pub const SANDBOX_CONFIG_FILENAME: &str = "sandbox.toml";

/// Legacy project-hook trust list (`$GROK_HOME/trusted-hook-projects`).
/// Migrated into [`TRUSTED_FOLDERS_FILENAME`] on the next unsandboxed start.
pub const TRUSTED_HOOK_PROJECTS_FILENAME: &str = "trusted-hook-projects";

/// Plugin trust list (`$GROK_HOME/trusted-plugins`).
pub const TRUSTED_PLUGINS_FILENAME: &str = "trusted-plugins";

pub fn load_managed_config() -> std::io::Result<toml::Value> {
    load_user_config_layer(user_distill_home().as_deref(), MANAGED_CONFIG_FILENAME)
}

/// Load a user-tier config layer from `<home>/<filename>`.
/// With no resolvable user home, returns an empty table rather than reading a cwd-relative `.grok/<filename>`.
/// The cwd fallback would silently promote an untrusted project `.grok` to the user tier.
fn load_user_config_layer(home: Option<&Path>, filename: &str) -> std::io::Result<toml::Value> {
    match home {
        Some(g) => load_config_file(&g.join(filename)),
        None => Ok(toml::Value::Table(toml::map::Map::new())),
    }
}

pub fn load_system_managed_config() -> std::io::Result<toml::Value> {
    let mut v = match system_config_dir() {
        Some(dir) => load_toml_file(&dir.join(MANAGED_CONFIG_FILENAME))?,
        None => toml::Value::Table(toml::map::Map::new()),
    };
    apply_version_overrides_with_registered(&mut v)?;
    Ok(v)
}

/// One managed-config layer: the parsed TOML and the file it came from.
#[derive(Debug, Clone)]
pub struct ManagedConfigLayer {
    pub value: toml::Value,
    pub path: std::path::PathBuf,
    /// `true` for the root-owned system layer (`/etc/grok`), derived from the load directory.
    pub is_system: bool,
}

/// All `managed_config.toml` layers in apply order (system first, user last).
/// Absent layers are skipped; unparsable layers are skipped with a warning.
/// One bad layer never drops the others.
pub fn managed_config_layers() -> Vec<ManagedConfigLayer> {
    managed_config_layers_at(system_config_dir().as_deref(), user_distill_home().as_deref())
}

/// [`managed_config_layers`] with explicit directories.
pub fn managed_config_layers_at(
    system_dir: Option<&Path>,
    user_home: Option<&Path>,
) -> Vec<ManagedConfigLayer> {
    let mut layers = Vec::new();
    for (dir, is_system) in [(system_dir, true), (user_home, false)] {
        let Some(path) = dir.map(|d| d.join(MANAGED_CONFIG_FILENAME)) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        match load_config_file(&path) {
            Ok(value) => layers.push(ManagedConfigLayer {
                value,
                path,
                is_system,
            }),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping managed_config.toml layer that failed to load or parse")
            }
        }
    }
    layers
}

/// A hook's origin (held by `distill_hooks::HookSpec::layer`).
/// Defined here, not in `distill-hooks`, since the dep direction is `distill-hooks -> distill-config`.
/// This crate sets the config tiers; `File`/`Plugin` are set downstream.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::AsRefStr,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum HookProvenance {
    /// `/etc/grok/managed_config.toml` (root-owned).
    SystemManaged,
    /// `$GROK_HOME/managed_config.toml` (server-synced, user-writable).
    Managed,
    /// System-tier `requirements.toml` (root-owned, e.g. `/etc/grok`).
    Requirements,
    /// `$GROK_HOME/requirements.toml` (user-writable).
    UserRequirements,
    /// `$GROK_HOME/config.toml`.
    User,
    /// A JSON hook file (the hooks directory, a vendor settings file, or a configured hooks path).
    File,
    /// A plugin-contributed hook.
    Plugin,
    /// A tier this build doesn't recognize (e.g. a newer peer's provenance over the wire).
    /// Forward-tolerant so an unknown value degrades to a conservative origin instead of failing the whole `HookRegistry` decode.
    #[serde(other)]
    Unknown,
}

/// Defaults to `File` so wire records written before provenance existed decode as the most conservative origin.
impl Default for HookProvenance {
    fn default() -> Self {
        Self::File
    }
}

impl HookProvenance {
    /// Root-owned admin policy tiers; the user cannot disable or skip their hooks.
    /// Every disable path must consult this predicate rather than re-derive the rule from names or paths.
    /// `$GROK_HOME` tiers (`Managed`, `UserRequirements`) never qualify: the user owns that directory and can rewrite or repoint it.
    pub fn is_managed_policy(self) -> bool {
        matches!(self, Self::SystemManaged | Self::Requirements)
    }

    /// Authority rank for duplicate resolution: when byte-identical hooks arrive from several tiers, the highest-ranked copy keeps its provenance.
    /// Deliberately NOT the config-merge precedence (where user overrides managed).
    /// Merge precedence answers "whose VALUE wins"; this answers "whose copy of one identical hook is authoritative": ownership, not recency.
    pub fn authority_rank(self) -> u8 {
        match self {
            Self::SystemManaged => 6,
            Self::Requirements => 5,
            Self::Managed => 4,
            Self::UserRequirements => 3,
            Self::User => 2,
            Self::File | Self::Plugin => 1,
            Self::Unknown => 0,
        }
    }
}

impl std::str::FromStr for HookProvenance {
    type Err = std::convert::Infallible;

    /// Inverse of [`HookProvenance`]'s strum string.
    /// Unrecognized strings map to [`HookProvenance::Unknown`] (forward-tolerant), so this never fails.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "system_managed" => Self::SystemManaged,
            "managed" => Self::Managed,
            "requirements" => Self::Requirements,
            "user_requirements" => Self::UserRequirements,
            "user" => Self::User,
            "file" => Self::File,
            "plugin" => Self::Plugin,
            _ => Self::Unknown,
        })
    }
}

/// One config layer's `hooks` subtree (read without `$VAR` expansion) plus its provenance.
#[derive(Debug, Clone)]
pub struct HookConfigLayer {
    provenance: HookProvenance,
    source_name: String,
    path: std::path::PathBuf,
    hooks: toml::Value,
}

impl HookConfigLayer {
    /// Construct a layer directly (in-memory config and tests); the synthesized `path` mirrors `source_name`.
    /// Real layers come from [`hook_config_layers`].
    pub fn new(
        provenance: HookProvenance,
        source_name: impl Into<String>,
        hooks: toml::Value,
    ) -> Self {
        let source_name = source_name.into();
        let path = std::path::PathBuf::from(&source_name);
        Self {
            provenance,
            source_name,
            path,
            hooks,
        }
    }

    pub fn provenance(&self) -> HookProvenance {
        self.provenance
    }

    /// A stable label for this layer (e.g. `"managed"`, `"requirements/user"`), used to prefix hook names for display and dedup.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// The layer's backing file, so parse errors can cite a real path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The raw `hooks` table, unexpanded so a literal `${VAR}` reaches the runner.
    pub fn hooks(&self) -> &toml::Value {
        &self.hooks
    }
}

/// All config-layer `hooks` blocks, highest authority first (matching [`effective_config_base`]).
/// Read WITHOUT env-expansion and never merged (hooks combine additively downstream).
/// Absent or unparsable layers are skipped with a warning so one bad layer can't drop the others.
pub fn hook_config_layers() -> Vec<HookConfigLayer> {
    hook_config_layers_at(system_config_dir().as_deref(), user_distill_home().as_deref())
}

/// Warn when a policy-tier hooks file is a symlink or not root-owned; the no-disable exemption assumes admin ownership of the system dir.
#[cfg(unix)]
fn warn_unless_root_owned(path: &Path) {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => tracing::warn!(
            path = %path.display(),
            "policy-tier hooks file is a symlink; its hooks cannot be disabled — ensure the target is admin-controlled"
        ),
        Ok(meta) if meta.uid() != 0 => tracing::warn!(
            path = %path.display(),
            uid = meta.uid(),
            "policy-tier hooks file is not root-owned; its hooks cannot be disabled — enforcement assumes admin ownership"
        ),
        // Root-owned but group/world-writable is the sneakier misconfig: any local user can edit the "non-disableable" policy
        Ok(meta) if meta.mode() & 0o022 != 0 => tracing::warn!(
            path = %path.display(),
            mode = format!("{:o}", meta.mode() & 0o777),
            "policy-tier hooks file is group- or world-writable; its hooks cannot be disabled — restrict write access to root"
        ),
        _ => {}
    }
}

#[cfg(not(unix))]
fn warn_unless_root_owned(_path: &Path) {}

/// [`hook_config_layers`] with explicit directories, for tests.
pub fn hook_config_layers_at(
    system_dir: Option<&Path>,
    user_home: Option<&Path>,
) -> Vec<HookConfigLayer> {
    /// One candidate config-hook layer: which directory and filename to read, and the provenance/label to stamp on hooks found there.
    struct LayerSpec<'a> {
        dir: Option<&'a Path>,
        filename: &'a str,
        provenance: HookProvenance,
        source_name: &'a str,
    }

    // Highest config authority first, matching `effective_config_base` precedence (requirements > user > managed > system_managed)
    // User overrides managed in this model
    // Byte-identical duplicates resolve by `HookProvenance::authority_rank` regardless of this order; every distinct hook runs regardless
    let specs = [
        LayerSpec {
            dir: system_dir,
            filename: REQUIREMENTS_FILENAME,
            provenance: HookProvenance::Requirements,
            source_name: "requirements/system",
        },
        LayerSpec {
            dir: user_home,
            filename: REQUIREMENTS_FILENAME,
            provenance: HookProvenance::UserRequirements,
            source_name: "requirements/user",
        },
        LayerSpec {
            dir: user_home,
            filename: USER_CONFIG_FILENAME,
            provenance: HookProvenance::User,
            source_name: "user",
        },
        LayerSpec {
            dir: user_home,
            filename: MANAGED_CONFIG_FILENAME,
            provenance: HookProvenance::Managed,
            source_name: "managed",
        },
        LayerSpec {
            dir: system_dir,
            filename: MANAGED_CONFIG_FILENAME,
            provenance: HookProvenance::SystemManaged,
            source_name: "system_managed",
        },
    ];

    let mut layers = Vec::new();
    for LayerSpec {
        dir,
        filename,
        provenance,
        source_name,
    } in specs
    {
        let Some(path) = dir.map(|d| d.join(filename)) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        // The no-disable exemption rests on OS ownership; a misconfigured system dir would silently create non-disableable hooks, so make it loud
        // Classification is unchanged (a root-owned deployment is the documented requirement, not something we can verify portably)
        if provenance.is_managed_policy() {
            warn_unless_root_owned(&path);
        }
        // No `$VAR` expansion: a literal `${VAR}` must reach the hook runner, which does the single expansion (expanding here would double-expand)
        let mut value = match read_toml_file(&path) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping config layer whose hooks could not be read");
                continue;
            }
        };
        // Apply `[[version_overrides]]` (parity with `load_config_file`); deep-merge only, no `$VAR` expansion, so the layer stays unexpanded
        if let Err(e) = apply_version_overrides_with_registered(&mut value) {
            tracing::warn!(path = %path.display(), error = %e, "skipping config layer whose version_overrides failed to apply");
            continue;
        }
        let Some(hooks) = value.get("hooks") else {
            continue;
        };
        if !hooks.is_table() {
            tracing::warn!(path = %path.display(), "ignoring non-table `hooks` value in config layer");
            continue;
        }
        layers.push(HookConfigLayer {
            provenance,
            source_name: source_name.to_string(),
            path: path.clone(),
            hooks: hooks.clone(),
        });
    }
    layers
}

/// Applies matching `[[version_overrides]]` patches against the running CLI version; strips the section either way.
/// If the installed version can't be parsed (broken `GROK_TEST_VERSION` in dev), it silently strips without applying, keeping the CLI usable.
pub fn apply_version_overrides_with_registered(value: &mut toml::Value) -> std::io::Result<()> {
    match distill_version::installed_semver() {
        Ok(version) => apply_version_overrides(value, &version)
            .map_err(|e| std::io::Error::other(e.redacted())),
        Err(_) => {
            if let Some(table) = value.as_table_mut() {
                table.remove(version_overrides::VERSION_OVERRIDES_KEY);
            }
            Ok(())
        }
    }
}

/// Normalize a single config layer in place, before it is merged with the others.
/// `deep_merge_toml` then replaces the whole policy from the winning layer instead of mixing keys across layers.
/// This runs on every input of the merge, not only the disk layers.
pub(crate) fn normalize_config_layer(layer: &mut toml::Value) {
    let Some(web_search) = layer
        .as_table_mut()
        .and_then(|t| t.get_mut("toolset"))
        .and_then(|t| t.as_table_mut())
        .and_then(|t| t.get_mut("web_search"))
        .and_then(|v| v.as_table_mut())
    else {
        return;
    };
    let non_empty = |table: &toml::value::Table, key: &str| {
        table
            .get(key)
            .and_then(toml::Value::as_array)
            .is_some_and(|a| !a.is_empty())
    };
    let allowed = non_empty(web_search, "allowed_domains");
    let excluded = non_empty(web_search, "excluded_domains");
    if allowed && !excluded {
        web_search.insert(
            "excluded_domains".to_string(),
            toml::Value::Array(Vec::new()),
        );
    } else if excluded && !allowed {
        web_search.insert(
            "allowed_domains".to_string(),
            toml::Value::Array(Vec::new()),
        );
    }
}

/// Recursively merge `overrides` into `base`. Values in `overrides` win.
pub fn deep_merge_toml(base: &mut toml::Value, overrides: &toml::Value) {
    if let toml::Value::Table(overrides_table) = overrides
        && let toml::Value::Table(base_table) = base
    {
        for (key, value) in overrides_table {
            if let Some(existing) = base_table.get_mut(key) {
                deep_merge_toml(existing, value);
            } else {
                base_table.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = overrides.clone();
    }
}

/// Expand `$VAR` / `${VAR}` in all string values.
pub fn expand_env_vars_in_toml(value: &mut toml::Value) {
    match value {
        toml::Value::String(s) => {
            let expanded = expand_env_vars_in_string(s);
            if expanded != *s {
                *s = expanded;
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                expand_env_vars_in_toml(item);
            }
        }
        toml::Value::Table(table) => {
            for (_, item) in table.iter_mut() {
                expand_env_vars_in_toml(item);
            }
        }
        _ => {}
    }
}

/// Expand `$VAR` / `${VAR}` in a single string.
pub fn expand_env_vars_in_string(input: &str) -> String {
    let context = |name: &str| std::env::var(name).ok();
    shellexpand::env_with_context_no_errors(input, context).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn hook_config_layers_reads_each_layer_unmerged_with_provenance() {
        let sys = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            "config.toml",
            "[[hooks.PreToolUse]]\nmatcher = \"Bash\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"${HOME}/u.sh\"\n",
        );
        write(
            home.path(),
            MANAGED_CONFIG_FILENAME,
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"/m.sh\"\n",
        );
        write(
            sys.path(),
            REQUIREMENTS_FILENAME,
            "[[hooks.PostToolUse]]\n[[hooks.PostToolUse.hooks]]\ntype = \"command\"\ncommand = \"/r.sh\"\n",
        );

        let layers = hook_config_layers_at(Some(sys.path()), Some(home.path()));

        // Highest authority first, each layer keeping its own provenance.
        let names: Vec<_> = layers.iter().map(|l| l.source_name().to_string()).collect();
        assert_eq!(names, vec!["requirements/system", "user", "managed"]);
        let Some(user_layer) = layers.get(1) else {
            panic!("expected user hook layer at index 1: {names:?}");
        };
        assert_eq!(user_layer.provenance(), HookProvenance::User);
        // Unmerged, and `${HOME}` stays literal (the runner expands, not the loader).
        let cmd = user_layer
            .hooks()
            .get("PreToolUse")
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("hooks"))
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("command"))
            .and_then(toml::Value::as_str);
        assert_eq!(cmd, Some("${HOME}/u.sh"));
    }

    /// The user-writable `$GROK_HOME/requirements.toml` stamps `UserRequirements`, never the exempt `Requirements`.
    /// A file the user owns cannot grant itself the no-disable exemption.
    #[test]
    fn user_requirements_layer_is_not_managed_policy() {
        let user_home = tempfile::tempdir().unwrap();
        std::fs::write(
            user_home.path().join("requirements.toml"),
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"x.sh\"\n",
        )
        .unwrap();
        let layers = hook_config_layers_at(None, Some(user_home.path()));
        assert_eq!(layers.len(), 1);
        let Some(layer) = layers.first() else {
            panic!("expected user requirements layer: {layers:?}");
        };
        assert_eq!(layer.provenance(), HookProvenance::UserRequirements);
        assert_eq!(layer.source_name(), "requirements/user");
        assert!(!layer.provenance().is_managed_policy());
    }

    #[test]
    fn hook_config_layers_bad_user_layer_does_not_drop_managed() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "config.toml", "this is = = not valid toml");
        write(
            home.path(),
            MANAGED_CONFIG_FILENAME,
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"/m.sh\"\n",
        );
        let layers = hook_config_layers_at(None, Some(home.path()));
        let names: Vec<_> = layers.iter().map(|l| l.source_name().to_string()).collect();
        assert_eq!(names, vec!["managed"]);
    }

    /// Direct contract for `deep_merge_toml`: nested tables merge (siblings preserved), arrays replace (not concatenate), missing keys insert.
    #[test]
    fn deep_merge_toml_table_merge_array_replace_and_insert() {
        let mut base: toml::Value = toml::from_str(
            r#"
            [features.telemetry]
            enabled = false
            sample_rate = 0.0

            [server]
            allowed = ["a", "b"]
            "#,
        )
        .unwrap();
        let overrides: toml::Value = toml::from_str(
            r#"
            [features.telemetry]
            enabled = true

            [server]
            allowed = ["c"]

            [brand_new]
            x = 1
            "#,
        )
        .unwrap();

        deep_merge_toml(&mut base, &overrides);

        assert_eq!(
            base.get("features")
                .and_then(|f| f.get("telemetry"))
                .and_then(|t| t.get("enabled"))
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            base.get("features")
                .and_then(|f| f.get("telemetry"))
                .and_then(|t| t.get("sample_rate"))
                .and_then(toml::Value::as_float),
            Some(0.0)
        );
        let arr: Vec<_> = base
            .get("server")
            .and_then(|s| s.get("allowed"))
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(arr, vec!["c"]);
        assert_eq!(
            base.get("brand_new")
                .and_then(|b| b.get("x"))
                .and_then(toml::Value::as_integer),
            Some(1)
        );
    }

    fn ws_layer(body: &str) -> toml::Value {
        toml::from_str(&format!("[toolset.web_search]\n{body}\n")).unwrap()
    }

    fn ws_array(v: &toml::Value, key: &str) -> Option<Vec<String>> {
        v.get("toolset")?
            .get("web_search")?
            .get(key)?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_str().map(str::to_owned))
                    .collect()
            })
    }

    #[test]
    fn normalize_sets_the_absent_sibling_to_empty() {
        let mut allow = ws_layer(r#"allowed_domains = ["a.com"]"#);
        normalize_config_layer(&mut allow);
        assert_eq!(
            ws_array(&allow, "allowed_domains"),
            Some(vec!["a.com".into()])
        );
        assert_eq!(ws_array(&allow, "excluded_domains"), Some(vec![]));

        let mut block = ws_layer(r#"excluded_domains = ["b.com"]"#);
        normalize_config_layer(&mut block);
        assert_eq!(
            ws_array(&block, "excluded_domains"),
            Some(vec!["b.com".into()])
        );
        assert_eq!(ws_array(&block, "allowed_domains"), Some(vec![]));
    }

    #[test]
    fn normalize_leaves_both_set_and_both_unset_untouched() {
        let mut both = ws_layer("allowed_domains = [\"a.com\"]\nexcluded_domains = [\"b.com\"]");
        normalize_config_layer(&mut both);
        assert_eq!(
            ws_array(&both, "allowed_domains"),
            Some(vec!["a.com".into()])
        );
        assert_eq!(
            ws_array(&both, "excluded_domains"),
            Some(vec!["b.com".into()])
        );

        let mut none: toml::Value = toml::from_str("[toolset.web_search]\n").unwrap();
        normalize_config_layer(&mut none);
        assert_eq!(ws_array(&none, "allowed_domains"), None);
        assert_eq!(ws_array(&none, "excluded_domains"), None);
    }

    /// After per-layer normalization, a plain `deep_merge_toml` lets a higher layer's blocklist beat a lower layer's allowlist atomically.
    #[test]
    fn normalized_layers_deep_merge_atomically() {
        let mut lower = ws_layer(r#"allowed_domains = ["github.com"]"#);
        let mut higher = ws_layer(r#"excluded_domains = ["evil.com"]"#);
        normalize_config_layer(&mut lower);
        normalize_config_layer(&mut higher);

        // higher wins in a deep merge
        let mut merged = lower;
        deep_merge_toml(&mut merged, &higher);

        assert_eq!(
            ws_array(&merged, "excluded_domains"),
            Some(vec!["evil.com".into()])
        );
        assert_eq!(
            ws_array(&merged, "allowed_domains"),
            Some(vec![]),
            "lower layer's allowlist must be cleared, not merged in"
        );
    }

    /// Campaign and version-override patches overlay after the layer merge, so they need the same normalization.
    /// A campaign that flips an allowlist to a blocklist must replace the policy, not leave both keys set.
    #[test]
    fn overlay_patches_are_normalized_before_merge() {
        let mut merged = ws_layer(r#"allowed_domains = ["github.com"]"#);
        normalize_config_layer(&mut merged);

        let patch: toml::Table =
            toml::from_str("[toolset.web_search]\nexcluded_domains = [\"evil.com\"]\n").unwrap();
        crate::config_override::apply_patches(
            &mut merged,
            std::iter::once(patch),
            crate::config_override::PATCH_STRIP_KEYS,
        );

        assert_eq!(
            ws_array(&merged, "excluded_domains"),
            Some(vec!["evil.com".into()])
        );
        assert_eq!(
            ws_array(&merged, "allowed_domains"),
            Some(vec![]),
            "the campaign's blocklist must replace the underlying allowlist"
        );
    }

    #[test]
    fn user_version_overrides_dont_escape_their_layer() {
        let cli_version = semver::Version::parse("1.8.0").unwrap();
        let mut user: toml::Value = toml::from_str(
            r#"
            [[version_overrides]]
            minimum_version = "1.0.0"
            [version_overrides.telemetry]
            mode = "enabled"
            "#,
        )
        .unwrap();
        apply_version_overrides(&mut user, &cli_version).unwrap();
        assert_eq!(
            user.get("telemetry")
                .and_then(|t| t.get("mode"))
                .and_then(toml::Value::as_str),
            Some("enabled")
        );

        let requirements: toml::Value = toml::from_str(
            r#"
            [telemetry]
            mode = "disabled"
            "#,
        )
        .unwrap();

        let mut merged = user;
        deep_merge_toml(&mut merged, &requirements);
        assert_eq!(
            merged
                .get("telemetry")
                .and_then(|t| t.get("mode"))
                .and_then(toml::Value::as_str),
            Some("disabled")
        );
    }

    #[test]
    fn load_user_config_layer_is_empty_without_user_home() {
        // No resolvable user home: no user layer, and no cwd-relative .grok read
        let v = load_user_config_layer(None, "config.toml").unwrap();
        assert_eq!(v.as_table().map(|t| t.is_empty()), Some(true));
    }

    #[test]
    fn load_user_config_layer_treats_empty_file_as_empty_table() {
        let dir = std::env::temp_dir().join(format!("grok-load-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), b"").unwrap();
        let v = load_user_config_layer(Some(&dir), "config.toml").unwrap();
        assert_eq!(v.as_table().map(|t| t.is_empty()), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_user_config_layer_reads_file_when_home_present() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("grok-load-layer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join("config.toml")).unwrap();
        writeln!(f, "[telemetry]\nmode = \"from_file\"\n").unwrap();

        let v = load_user_config_layer(Some(&dir), "config.toml").unwrap();
        assert_eq!(
            v.get("telemetry")
                .and_then(|t| t.get("mode"))
                .and_then(toml::Value::as_str),
            Some("from_file")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The returned error keeps the parser's kind and location but never the source snippet, which can carry a secret and would reach clients.
    #[test]
    fn parse_error_keeps_kind_but_not_snippet() {
        let dir = std::env::temp_dir().join(format!("grok-toml-leak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        // Duplicate key: the message names the key; the secret-bearing source line is only in Display.
        std::fs::write(
            &path,
            "api_key = \"xai-secretmustnotleak\"\napi_key = \"xai-secretmustnotleak2\"\n",
        )
        .unwrap();

        let msg = load_toml_file(&path).unwrap_err().to_string();
        assert!(
            msg.contains("TOML parse error at line 2"),
            "want location: {msg}"
        );
        assert!(msg.contains("duplicate key"), "want parser kind: {msg}");
        assert!(
            !msg.contains("xai-secretmustnotleak"),
            "leaked the secret value: {msg}"
        );
        assert!(
            !msg.contains('|') && !msg.contains('^'),
            "leaked the source snippet/caret: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
