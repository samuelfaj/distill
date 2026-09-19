// Modified for Distill by Samuel Fajreldines, 2026.
use super::RemoteSettings;
use distill_fast_worktree::CreationMode;
use serde::{Deserialize, Serialize};
use toml::Value as TomlValue;

/// Mirrors the internal `CreationMode` enum from distill-fast-worktree but uses config-friendly naming (lowercase strings in TOML).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeType {
    /// Linked worktree via `git worktree add --no-checkout` and a parallel CoW copy.
    /// This is the fastest mode for large repos.
    #[default]
    Linked,
    /// Standalone repository copy with independent `.git/` directory.
    /// Can be promoted to replace the source via `rename()`.
    Standalone,
    /// Plain `git worktree add` with full checkout.
    Git,
}

impl std::str::FromStr for WorktreeType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "linked" => Ok(Self::Linked),
            "standalone" => Ok(Self::Standalone),
            "git" => Ok(Self::Git),
            _ => Err(()),
        }
    }
}

impl From<WorktreeType> for CreationMode {
    fn from(t: WorktreeType) -> Self {
        match t {
            WorktreeType::Linked => CreationMode::Linked,
            WorktreeType::Standalone => CreationMode::Standalone,
            WorktreeType::Git => CreationMode::GitCheckout,
        }
    }
}

/// Returns `Some(type)` when `[cli] worktree_type` is set to a valid value in config.toml,
/// `None` when absent or the value type is wrong. Logs a warning for invalid strings.
pub(crate) fn worktree_type_from_toml_opt(root: &TomlValue) -> Option<WorktreeType> {
    if let TomlValue::Table(table) = root
        && let Some(TomlValue::Table(cli)) = table.get("cli")
        && let Some(toml_value) = cli.get("worktree_type")
    {
        if let Some(type_str) = toml_value.as_str() {
            return match type_str.parse::<WorktreeType>() {
                Ok(wt) => Some(wt),
                Err(()) => {
                    tracing::warn!("Invalid worktree_type value in config: {type_str}, ignoring");
                    None
                }
            };
        }
        tracing::warn!("Invalid worktree_type value in config: {toml_value:?}, ignoring");
    }
    None
}

pub(crate) fn worktree_type_from_toml(root: &TomlValue) -> WorktreeType {
    worktree_type_from_toml_opt(root).unwrap_or_default()
}

/// Resolve worktree type: local config > remote settings > default (`Linked`).
pub(crate) fn resolve_worktree_type(
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> (WorktreeType, &'static str) {
    if let Some(wt) = worktree_type_from_toml_opt(raw_config) {
        return (wt, "local");
    }
    if let Some(s) = remote.and_then(|r| r.worktree_type.as_deref()) {
        match s.parse::<WorktreeType>() {
            Ok(wt) => return (wt, "remote"),
            Err(()) => {
                tracing::warn!("Invalid remote worktree_type: {s}, using default");
            }
        }
    }
    (WorktreeType::default(), "default")
}

pub fn worktree_type() -> WorktreeType {
    let root: TomlValue = match crate::config::load_effective_config() {
        Ok(r) => r,
        Err(_) => return WorktreeType::Linked,
    };
    worktree_type_from_toml(&root)
}

/// Env override for grove vs copy (`grove` | `grove-fuse` | `grove-nfs` | `grove-projfs` | `nfs` | `copy`).
/// Distinct from [`WorktreeType`] (`linked` | `standalone` | `git`).
pub const ENV_WORKTREE_TYPE: &str = "GROK_WORKTREE_TYPE";

/// Convenience that enables both `grok clone` and session / `-w` Grove when specific knobs are unset.
pub const ENV_GROVE: &str = "GROK_GROVE";

fn grove_from_str(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "grove" | "grove-fuse" | "grove-nfs" | "grove-projfs" | "nfs" | "true" | "1" | "on" => {
            Some(true)
        }
        "copy" | "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

fn grove_enable_all_from_str(s: &str) -> Option<bool> {
    if s.trim().eq_ignore_ascii_case("all") {
        return Some(true);
    }
    grove_from_str(s)
}

fn grove_worktree_from_toml_opt(root: &TomlValue) -> Option<bool> {
    let cli = root.get("cli")?;
    for key in ["grove_worktree", "nfs_worktree"] {
        if let Some(v) = cli.get(key) {
            if let Some(b) = v.as_bool() {
                return Some(b);
            }
            if let Some(s) = v.as_str() {
                return grove_from_str(s);
            }
            tracing::warn!("Invalid [cli].{key} value: {v:?}, ignoring");
        }
    }
    if let Some(s) = cli.get("worktree_type").and_then(|v| v.as_str()) {
        match s {
            "grove" | "grove-fuse" | "grove-nfs" | "grove-projfs" | "nfs" => return Some(true),
            "copy" => return Some(false),
            _ => {}
        }
    }
    None
}

pub fn grove_enable_all_from_toml(root: &TomlValue) -> Option<bool> {
    let v = root.get("cli")?.get("grove")?;
    if let Some(b) = v.as_bool() {
        return Some(b);
    }
    if let Some(s) = v.as_str() {
        return grove_enable_all_from_str(s);
    }
    tracing::warn!("Invalid [cli].grove value: {v:?}, ignoring");
    None
}

/// Remote `grove_worktree = false` kills last; a missing remote is not a refusal.
pub fn resolve_grove_worktree(
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> (bool, &'static str) {
    gate_grove_worktree(None, raw_config, remote)
}

pub fn grove_worktree_env() -> Option<bool> {
    std::env::var(ENV_WORKTREE_TYPE)
        .ok()
        .and_then(|s| grove_from_str(&s))
}

pub fn grove_enable_all_env() -> Option<bool> {
    std::env::var(ENV_GROVE)
        .ok()
        .and_then(|s| grove_enable_all_from_str(&s))
}

/// Env `GROK_GROVE` first; a set false skips `[cli] grove` rather than forcing both surfaces off.
pub fn grove_enable_all_asked(env: Option<bool>, raw_config: &TomlValue) -> bool {
    env.unwrap_or_else(|| grove_enable_all_from_toml(raw_config).unwrap_or(false))
}

/// Resolves request → env → local → enable-all → remote-true; remote kill last.
/// Tests inject `env` and `enable_all` instead of mutating process state.
pub fn gate_grove_worktree_layers(
    desired: Option<bool>,
    env: Option<bool>,
    enable_all: Option<bool>,
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> (bool, &'static str) {
    let mut enabled = false;
    let mut src = "default";
    if let Some(v) = desired {
        enabled = v;
        src = "request";
    } else if let Some(v) = env {
        enabled = v;
        src = "env";
    } else if let Some(v) = grove_worktree_from_toml_opt(raw_config) {
        enabled = v;
        src = "local";
    } else if grove_enable_all_asked(enable_all, raw_config) {
        enabled = true;
        src = "enable_all";
    } else if remote.and_then(|r| r.grove_worktree) == Some(true) {
        enabled = true;
        src = "remote";
    }
    if remote.is_some_and(|r| r.grove_worktree == Some(false)) {
        // A kill is only the *reason* when a layer actually asked for grove.
        return (false, if enabled { "remote_kill" } else { src });
    }
    (enabled, src)
}

/// Single grove-vs-copy gate. `desired` is an explicit client/resume flag.
pub fn gate_grove_worktree(
    desired: Option<bool>,
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> (bool, &'static str) {
    gate_grove_worktree_layers(
        desired,
        grove_worktree_env(),
        grove_enable_all_env(),
        raw_config,
        remote,
    )
}

/// Same gate as session create, with no client request slot (subagent spawn,
/// rehydrate). Does not emit `WORKTREE_REQUEST_SHELL`.
pub fn grove_worktree_gate(remote: Option<&RemoteSettings>) -> (bool, &'static str) {
    let root: TomlValue = match crate::config::load_effective_config() {
        Ok(r) => r,
        Err(_) => TomlValue::Table(toml::map::Map::new()),
    };
    gate_grove_worktree(None, &root, remote)
}

pub fn grove_worktree_enabled(remote: Option<&RemoteSettings>) -> bool {
    grove_worktree_gate(remote).0
}

/// Same `NfsWorktreeOpts` as session create (`enabled_grove_opts` in distill-workspace).
/// `None` leaves the Grove arm off (`WorktreeBuilder` default).
pub(crate) fn grove_worktree_opts_if_enabled(
    enabled: bool,
) -> Option<distill_fast_worktree::NfsWorktreeOpts> {
    enabled.then(|| distill_fast_worktree::NfsWorktreeOpts {
        enabled: true,
        ..distill_fast_worktree::NfsWorktreeOpts::default()
    })
}

pub(crate) fn restore_code_from_toml(root: &TomlValue) -> Option<bool> {
    root.get("cli")
        .and_then(|c| c.get("restore_code"))
        .and_then(|v| v.as_bool())
}

/// Used when the client omits `restoreCode` on the wire.
pub(crate) fn resolve_restore_code(
    raw_config: &TomlValue,
    remote: Option<&RemoteSettings>,
) -> bool {
    restore_code_from_toml(raw_config)
        .or(remote.and_then(|r| r.restore_code))
        .unwrap_or(false)
}

/// Resolve `[worktree.auto_gc]` from parsed settings: env > local > remote > defaults (clamped).
/// Platform age policy is applied later in `maybe_auto_gc`.
/// (`distill-fast-worktree`'s `resolve_worktree_auto_gc_from_layers` owns and tests precedence and clamping; this only maps settings to layers.)
pub(crate) fn resolve_worktree_auto_gc_from_settings(
    local: Option<&super::WorktreeAutoGcSettings>,
    remote: Option<&super::WorktreeAutoGcSettings>,
) -> distill_fast_worktree::ResolvedWorktreeAutoGc {
    use distill_workspace::worktree::worktree_auto_gc_layer_from_settings;
    let local_layer = local.map(worktree_auto_gc_layer_from_settings);
    let remote_layer = remote.map(worktree_auto_gc_layer_from_settings);
    distill_fast_worktree::resolve_worktree_auto_gc_from_layers(
        local_layer.as_ref(),
        remote_layer.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    use super::RemoteSettings;
    use super::*;
    use serial_test::serial;
    use toml::Value as TomlValue;

    #[test]
    fn test_worktree_type_linked() {
        let toml_str = r#"
[cli]
worktree_type = "linked"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_standalone() {
        let toml_str = r#"
[cli]
worktree_type = "standalone"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Standalone);
    }

    #[test]
    fn test_worktree_type_git() {
        let toml_str = r#"
[cli]
worktree_type = "git"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Git);
    }

    #[test]
    fn test_worktree_type_default_linked() {
        let toml_str = r#"
[cli]
auto_update = true
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_no_cli_section() {
        let toml_str = r#"
[models]
default = "grok-code-fast-1"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_invalid_value() {
        let toml_str = r#"
[cli]
worktree_type = "invalid"
"#;
        let root: TomlValue = toml::from_str(toml_str).unwrap();
        assert_eq!(worktree_type_from_toml(&root), WorktreeType::Linked);
    }

    #[test]
    fn test_worktree_type_fromstr() {
        assert_eq!("linked".parse::<WorktreeType>(), Ok(WorktreeType::Linked));
        assert_eq!(
            "standalone".parse::<WorktreeType>(),
            Ok(WorktreeType::Standalone)
        );
        assert_eq!("git".parse::<WorktreeType>(), Ok(WorktreeType::Git));
        assert!("invalid".parse::<WorktreeType>().is_err());
        assert!("LINKED".parse::<WorktreeType>().is_err());
    }

    #[test]
    fn test_worktree_type_from_toml_opt_present() {
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"standalone\"").unwrap();
        assert_eq!(
            worktree_type_from_toml_opt(&root),
            Some(WorktreeType::Standalone)
        );
    }

    #[test]
    fn test_worktree_type_from_toml_opt_absent() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert_eq!(worktree_type_from_toml_opt(&root), None);
    }

    #[test]
    fn test_worktree_type_from_toml_opt_invalid() {
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"bogus\"").unwrap();
        assert_eq!(worktree_type_from_toml_opt(&root), None);
    }

    #[test]
    fn test_worktree_type_from_toml_opt_no_cli_section() {
        let root: TomlValue = toml::from_str("[models]\ndefault = \"grok\"").unwrap();
        assert_eq!(worktree_type_from_toml_opt(&root), None);
    }

    #[test]
    fn test_resolve_worktree_type_local_wins_over_remote() {
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"git\"").unwrap();
        let remote = RemoteSettings {
            worktree_type: Some("standalone".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Git, "local")
        );
    }

    #[test]
    fn test_resolve_worktree_type_remote_fallback() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            worktree_type: Some("standalone".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Standalone, "remote")
        );
    }

    #[test]
    fn test_resolve_worktree_type_default_when_no_config() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert_eq!(
            resolve_worktree_type(&root, None),
            (WorktreeType::Linked, "default")
        );
    }

    #[test]
    fn test_resolve_worktree_type_invalid_remote_falls_back_to_default() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            worktree_type: Some("bogus".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Linked, "default")
        );
    }

    #[test]
    fn test_resolve_worktree_type_remote_none_field() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            worktree_type: None,
            ..Default::default()
        };
        assert_eq!(
            resolve_worktree_type(&root, Some(&remote)),
            (WorktreeType::Linked, "default")
        );
    }

    fn clear_worktree_type_env() {
        unsafe { std::env::remove_var(ENV_WORKTREE_TYPE) };
        unsafe { std::env::remove_var(ENV_GROVE) };
    }

    fn remote_unset() -> RemoteSettings {
        RemoteSettings {
            grove_worktree: None,
            ..Default::default()
        }
    }

    #[test]
    #[serial]
    fn resolve_grove_worktree_default_copy() {
        clear_worktree_type_env();
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote_unset())),
            (false, "default")
        );
        assert_eq!(resolve_grove_worktree(&root, None), (false, "default"));
    }

    #[test]
    #[serial]
    fn resolve_grove_worktree_toml_bool_and_type_spelling() {
        clear_worktree_type_env();
        let remote = remote_unset();
        let root: TomlValue = toml::from_str("[cli]\ngrove_worktree = true").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (true, "local")
        );
        let root: TomlValue = toml::from_str("[cli]\nnfs_worktree = true").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (true, "local")
        );
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"grove\"").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (true, "local")
        );
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"nfs\"").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (true, "local")
        );
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"copy\"").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (false, "local")
        );
        let root: TomlValue = toml::from_str("[cli]\nworktree_type = \"linked\"").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (false, "default")
        );
        let root: TomlValue =
            toml::from_str("[cli]\ngrove_worktree = false\nnfs_worktree = true").unwrap();
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (false, "local")
        );
    }

    #[test]
    #[serial]
    fn resolve_grove_worktree_env_wins_over_local() {
        clear_worktree_type_env();
        let remote = remote_unset();
        unsafe { std::env::set_var(ENV_WORKTREE_TYPE, "grove") };
        let root: TomlValue = toml::from_str("[cli]\ngrove_worktree = false").unwrap();
        assert_eq!(resolve_grove_worktree(&root, Some(&remote)), (true, "env"));
        unsafe { std::env::set_var(ENV_WORKTREE_TYPE, "copy") };
        let root: TomlValue = toml::from_str("[cli]\ngrove_worktree = true").unwrap();
        assert_eq!(resolve_grove_worktree(&root, Some(&remote)), (false, "env"));
        clear_worktree_type_env();
    }

    #[test]
    fn gate_grove_worktree_layers_resume_parity_without_process_env() {
        let local_grove: TomlValue = toml::from_str("[cli]\ngrove_worktree = true").unwrap();
        let empty: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote_unset = remote_unset();
        let remote_kill = RemoteSettings {
            grove_worktree: Some(false),
            ..Default::default()
        };

        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &local_grove, Some(&remote_unset)),
            (true, "local"),
            "remote Some(unset) + local grove must enable, same as create"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &local_grove, None),
            (true, "local"),
            "local opt-in must survive missing remote settings"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, Some(true), None, &empty, Some(&remote_unset)),
            (true, "env"),
            "remote Some(unset) + env grove must enable, same as create"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, Some(true), None, &empty, Some(&remote_kill)),
            (false, "remote_kill"),
            "remote kill must disable even when env asked for grove"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, Some(true), None, &empty, None),
            (true, "env"),
            "env opt-in must survive missing remote settings"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &empty, None),
            (false, "default"),
            "a missing remote must not be reported as a refused grove request"
        );
        assert_eq!(
            gate_grove_worktree_layers(Some(false), None, None, &local_grove, Some(&remote_kill)),
            (false, "request"),
            "the kill switch must not claim a request that asked for copy"
        );
    }

    #[test]
    fn inherited_subagent_grove_opts_follow_gate_layers() {
        let local_grove: TomlValue = toml::from_str("[cli]\ngrove_worktree = true").unwrap();
        let empty: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote_kill = RemoteSettings {
            grove_worktree: Some(false),
            ..RemoteSettings::default()
        };

        let (on, src) = gate_grove_worktree_layers(None, None, None, &local_grove, None);
        assert_eq!((on, src), (true, "local"));
        assert!(
            grove_worktree_opts_if_enabled(on).is_some_and(|opts| opts.enabled),
            "inherited local gate must attach enabled Grove opts"
        );

        let (off, src) = gate_grove_worktree_layers(None, None, None, &empty, None);
        assert_eq!((off, src), (false, "default"));
        assert!(
            grove_worktree_opts_if_enabled(off).is_none(),
            "default-off must not attach Grove opts"
        );

        let (killed, src) =
            gate_grove_worktree_layers(None, Some(true), None, &empty, Some(&remote_kill));
        assert_eq!((killed, src), (false, "remote_kill"));
        assert!(
            grove_worktree_opts_if_enabled(killed).is_none(),
            "remote kill must not attach Grove opts"
        );
    }

    #[test]
    fn gate_grove_worktree_layers_enable_all_and_overrides() {
        let empty: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let enable_all_toml: TomlValue = toml::from_str("[cli]\ngrove = true").unwrap();
        let specific_off: TomlValue =
            toml::from_str("[cli]\ngrove = true\ngrove_worktree = false").unwrap();
        let remote_unset = remote_unset();
        let remote_kill = RemoteSettings {
            grove_worktree: Some(false),
            ..Default::default()
        };
        let remote_true = RemoteSettings {
            grove_worktree: Some(true),
            ..Default::default()
        };

        assert_eq!(
            gate_grove_worktree_layers(None, None, Some(true), &empty, Some(&remote_unset)),
            (true, "enable_all")
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &enable_all_toml, Some(&remote_unset)),
            (true, "enable_all")
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &enable_all_toml, None),
            (true, "enable_all"),
            "enable-all must survive missing remote"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, Some(false), Some(true), &empty, Some(&remote_unset)),
            (false, "env"),
            "GROK_WORKTREE_TYPE=copy must beat enable-all"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &specific_off, Some(&remote_unset)),
            (false, "local"),
            "[cli] grove_worktree = false must beat [cli] grove"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, Some(true), &empty, Some(&remote_kill)),
            (false, "remote_kill"),
            "remote kill must win over enable-all"
        );
        assert_eq!(
            gate_grove_worktree_layers(
                None,
                None,
                Some(false),
                &enable_all_toml,
                Some(&remote_unset)
            ),
            (false, "default"),
            "GROK_GROVE=off must skip [cli] grove and fall through"
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, Some(false), &empty, Some(&remote_true)),
            (true, "remote")
        );
        assert_eq!(
            gate_grove_worktree_layers(Some(true), None, None, &empty, None),
            (true, "request")
        );
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &empty, Some(&remote_unset)),
            (false, "default")
        );
        let all_spelling: TomlValue = toml::from_str("[cli]\ngrove = \"all\"").unwrap();
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &all_spelling, Some(&remote_unset)),
            (true, "enable_all")
        );
        let copy_spelling: TomlValue = toml::from_str("[cli]\ngrove = \"copy\"").unwrap();
        assert_eq!(grove_enable_all_from_toml(&copy_spelling), Some(false));
        assert_eq!(
            gate_grove_worktree_layers(None, None, None, &copy_spelling, Some(&remote_unset)),
            (false, "default")
        );
        assert!(!grove_enable_all_asked(Some(false), &enable_all_toml));
        assert!(grove_enable_all_asked(None, &enable_all_toml));
        let invalid: TomlValue = toml::from_str("[cli]\ngrove = \"maybe\"").unwrap();
        assert_eq!(grove_enable_all_from_toml(&invalid), None);
        assert_eq!(grove_enable_all_from_toml(&enable_all_toml), Some(true));
        let enable_all_off: TomlValue = toml::from_str("[cli]\ngrove = false").unwrap();
        assert_eq!(grove_enable_all_from_toml(&enable_all_off), Some(false));
    }

    #[test]
    #[serial]
    fn gate_grove_worktree_kill_switch_wins_over_request() {
        clear_worktree_type_env();
        let root: TomlValue = toml::from_str("[cli]\ngrove_worktree = true").unwrap();
        let remote = RemoteSettings {
            grove_worktree: Some(false),
            ..Default::default()
        };
        assert_eq!(
            gate_grove_worktree(Some(true), &root, Some(&remote)),
            (false, "remote_kill")
        );
        unsafe { std::env::set_var(ENV_WORKTREE_TYPE, "grove") };
        assert_eq!(
            gate_grove_worktree(Some(true), &root, None),
            (true, "request")
        );
        clear_worktree_type_env();
    }

    #[test]
    #[serial]
    fn resolve_grove_worktree_remote_kill_switch_wins() {
        clear_worktree_type_env();
        unsafe { std::env::set_var(ENV_WORKTREE_TYPE, "grove") };
        let root: TomlValue = toml::from_str("[cli]\ngrove_worktree = true").unwrap();
        let remote = RemoteSettings {
            grove_worktree: Some(false),
            ..Default::default()
        };
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (false, "remote_kill")
        );
        clear_worktree_type_env();
    }

    #[test]
    #[serial]
    fn resolve_grove_worktree_remote_true_when_unset() {
        clear_worktree_type_env();
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            grove_worktree: Some(true),
            ..Default::default()
        };
        assert_eq!(
            resolve_grove_worktree(&root, Some(&remote)),
            (true, "remote")
        );
    }

    #[test]
    fn remote_settings_deserializes_nfs_worktree_alias() {
        let s: RemoteSettings = serde_json::from_str(r#"{"nfs_worktree":false}"#).unwrap();
        assert_eq!(s.grove_worktree, Some(false));
        let s: RemoteSettings = serde_json::from_str(r#"{"grove_worktree":true}"#).unwrap();
        assert_eq!(s.grove_worktree, Some(true));
    }

    // === restore_code config tests ===

    #[test]
    fn test_restore_code_from_toml_present_true() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = true").unwrap();
        assert_eq!(restore_code_from_toml(&root), Some(true));
    }

    #[test]
    fn test_restore_code_from_toml_present_false() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = false").unwrap();
        assert_eq!(restore_code_from_toml(&root), Some(false));
    }

    #[test]
    fn test_restore_code_from_toml_absent() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert_eq!(restore_code_from_toml(&root), None);
    }

    #[test]
    fn test_restore_code_from_toml_no_cli_section() {
        let root: TomlValue = toml::from_str("[models]\ndefault = \"grok\"").unwrap();
        assert_eq!(restore_code_from_toml(&root), None);
    }

    #[test]
    fn test_restore_code_from_toml_wrong_type() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = \"yes\"").unwrap();
        assert_eq!(restore_code_from_toml(&root), None);
    }

    #[test]
    fn test_resolve_restore_code_local_wins_over_remote() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = true").unwrap();
        let remote = RemoteSettings {
            restore_code: Some(false),
            ..Default::default()
        };
        assert!(resolve_restore_code(&root, Some(&remote)));
    }

    #[test]
    fn test_resolve_restore_code_remote_fallback() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            restore_code: Some(true),
            ..Default::default()
        };
        assert!(resolve_restore_code(&root, Some(&remote)));
    }

    #[test]
    fn test_resolve_restore_code_default_false() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        assert!(!resolve_restore_code(&root, None));
    }

    #[test]
    fn test_resolve_restore_code_remote_none_falls_to_default() {
        let root: TomlValue = toml::from_str("[cli]\nauto_update = true").unwrap();
        let remote = RemoteSettings {
            restore_code: None,
            ..Default::default()
        };
        assert!(!resolve_restore_code(&root, Some(&remote)));
    }

    #[test]
    fn test_resolve_restore_code_local_false_overrides_remote_true() {
        let root: TomlValue = toml::from_str("[cli]\nrestore_code = false").unwrap();
        let remote = RemoteSettings {
            restore_code: Some(true),
            ..Default::default()
        };
        assert!(!resolve_restore_code(&root, Some(&remote)));
    }
}
