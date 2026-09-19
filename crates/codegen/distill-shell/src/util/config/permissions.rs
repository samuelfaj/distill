// Modified for Distill by Samuel Fajreldines, 2026.
use toml::Value as TomlValue;

pub use distill_telemetry::enums::PermissionMode;

/// Unknown strings fall back to `Ask` (safe direction: no YOLO on garbage).
/// The `"ask"` and `"default"` arms are explicit so a future `Default` variant is a one-line change without touching the catch-all.
pub fn parse_permission_mode_canonical(mode_str: &str) -> PermissionMode {
    match mode_str {
        "always-approve" => PermissionMode::AlwaysApprove,
        "auto" => PermissionMode::Auto,
        "ask" => PermissionMode::Ask,
        "default" => PermissionMode::Ask,
        _ => PermissionMode::Ask,
    }
}

/// Canonical `[ui] permission_mode` string for a resolved [`PermissionMode`].
///
/// Inverse of [`parse_permission_mode_canonical`] for the real variants, so `parse_permission_mode_canonical(permission_mode_canonical_str(m)) == m`.
pub(crate) fn permission_mode_canonical_str(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AlwaysApprove => "always-approve",
        PermissionMode::Auto => "auto",
        PermissionMode::Ask => "ask",
    }
}

/// Keys under `[ui]` that count as an explicit permission-mode setting.
const UI_PERMISSION_MODE_KEYS: &[&str] = &["permission_mode", "approval_mode", "yolo"];

/// Returns `Some` if `permission_mode`, legacy `approval_mode`, or legacy `yolo` is present.
/// Even `yolo = false` returns `Some(Ask)` so remote cannot win.
pub fn permission_mode_from_ui_if_set(ui: &TomlValue) -> Option<PermissionMode> {
    let table = ui.as_table()?;
    if !UI_PERMISSION_MODE_KEYS
        .iter()
        .any(|k| table.contains_key(*k))
    {
        return None;
    }

    if let Some(mode_str) = table.get("permission_mode").and_then(|v| v.as_str()) {
        return Some(parse_permission_mode_canonical(mode_str));
    }

    if let Some(mode_str) = table.get("approval_mode").and_then(|v| v.as_str()) {
        return Some(match mode_str {
            "always-approve" => PermissionMode::AlwaysApprove,
            _ => PermissionMode::Ask,
        });
    }

    if table.get("yolo").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Some(PermissionMode::AlwaysApprove);
    }

    Some(PermissionMode::Ask)
}

/// Shipped TUI default when CLI, TOML, and remote all leave the mode unset.
pub(crate) const DEFAULT_INTERACTIVE_PERMISSION_MODE: PermissionMode = PermissionMode::Ask;

pub(crate) const ENV_DEFAULT_PERMISSION_MODE: &str = "GROK_DEFAULT_PERMISSION_MODE";

/// Env override for [`DEFAULT_INTERACTIVE_PERMISSION_MODE`].
/// `always-approve` and unknown values are ignored so bypass cannot inherit from the process environment.
pub fn default_interactive_permission_mode() -> PermissionMode {
    match std::env::var(ENV_DEFAULT_PERMISSION_MODE).ok().as_deref() {
        Some("auto") => PermissionMode::Auto,
        Some("ask") | Some("default") => PermissionMode::Ask,
        _ => DEFAULT_INTERACTIVE_PERMISSION_MODE,
    }
}

/// TOML `[ui]` permission keys win, else remote. Returns `None` if neither chose a mode.
pub fn selected_permission_mode(
    effective_ui: Option<&TomlValue>,
    remote_permission_mode: Option<&str>,
) -> Option<PermissionMode> {
    if let Some(ui) = effective_ui
        && let Some(mode) = permission_mode_from_ui_if_set(ui)
    {
        return Some(mode);
    }
    remote_permission_mode.map(parse_permission_mode_canonical)
}

/// Selected mode, or Ask. This is the headless and display fallback; the interactive default does not apply.
pub(crate) fn resolve_permission_mode(
    effective_ui: Option<&TomlValue>,
    remote_permission_mode: Option<&str>,
) -> PermissionMode {
    selected_permission_mode(effective_ui, remote_permission_mode).unwrap_or(PermissionMode::Ask)
}

/// Display string for a selected mode that did NOT win yolo/auto enforcement.
/// AlwaysApprove (policy pin) and Auto (feature gate off) show as Ask so the UI never claims more than enforcement grants.
pub fn clamped_display_permission_mode(mode: PermissionMode) -> &'static str {
    if mode.is_always_approve() || mode.is_auto() {
        "ask"
    } else {
        permission_mode_canonical_str(mode)
    }
}

/// Displayed mode for a non-CLI resolution (effective TOML > remote > Ask), clamped per [`clamped_display_permission_mode`].
/// A persisted `"default"` keeps its distinct spelling (own settings option; enforcement equals Ask).
/// Only the `permission_mode` key can spell it and that key has top precedence, so the raw check before canonicalization is sufficient.
pub fn resolved_display_permission_mode(
    effective_ui: Option<&TomlValue>,
    remote_permission_mode: Option<&str>,
) -> &'static str {
    let toml_spelling = effective_ui
        .and_then(|ui| ui.as_table())
        .and_then(|t| t.get("permission_mode"))
        .and_then(|v| v.as_str());
    if toml_spelling == Some("default") {
        return "default";
    }
    let mode = resolve_permission_mode(effective_ui, remote_permission_mode);
    clamped_display_permission_mode(mode)
}

/// Load selected permission mode for launch (overlay-free TOML and explicit remote).
/// Missing or unknown values fall back to Ask; so does a config load failure.
pub fn load_permission_mode(remote_permission_mode: Option<&str>) -> PermissionMode {
    load_selected_permission_mode(remote_permission_mode).unwrap_or(PermissionMode::Ask)
}

/// Disk form of [`selected_permission_mode`].
/// Load failure is explicit Ask so a broken config cannot fall into the interactive default (which may be auto).
fn load_selected_permission_mode(remote_permission_mode: Option<&str>) -> Option<PermissionMode> {
    let layers = match crate::config::ConfigLayers::load() {
        Ok(l) => l,
        Err(_) => return Some(PermissionMode::Ask),
    };
    selected_permission_mode_from_layers(&layers, remote_permission_mode)
}

fn selected_permission_mode_from_layers(
    layers: &crate::config::ConfigLayers,
    remote: Option<&str>,
) -> Option<PermissionMode> {
    let merged = layers.effective_config_base_without_overlay();
    let ui = merged.as_table().and_then(|t| t.get("ui"));
    selected_permission_mode(ui, remote)
}

/// What production callers inline (the display path, `load_permission_mode`): select a mode, then fall back to Ask.
#[cfg(test)]
fn permission_mode_from_layers(
    layers: &crate::config::ConfigLayers,
    remote: Option<&str>,
) -> PermissionMode {
    selected_permission_mode_from_layers(layers, remote).unwrap_or(PermissionMode::Ask)
}

/// Result of [`effective_yolo_for_launch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveYolo {
    /// Client-side auto-approve for this launch.
    pub yolo: bool,
    /// Warning to show when a requested bypass was neutralized by the pin.
    pub blocked_warning: Option<&'static str>,
    /// The pin snapshot, set even when no bypass was requested, so callers reuse it.
    pub policy_block: Option<&'static str>,
}

/// Effective client-side yolo for the launch: CLI `--permission-mode`/`--yolo` beat `[ui] permission_mode`, and the policy pin force-disables either.
///
/// `remote_permission_mode` applies only when no TOML permission key is set; pass `None` when remote settings are unavailable.
pub fn effective_yolo_for_launch(
    cli_always_approve: bool,
    cli_permission_mode: Option<&str>,
    remote_permission_mode: Option<&str>,
) -> EffectiveYolo {
    let config_yolo = load_permission_mode(remote_permission_mode).is_always_approve();
    resolve_launch_yolo(
        resolve_effective_yolo(cli_always_approve, cli_permission_mode, config_yolo),
        yolo_disabled_by_policy(),
    )
}

/// Whether this launch should start in auto (not always-approve).
/// CLI `--permission-mode auto` beats config; yolo wins if both requested.
/// `unset_default` applies only when nothing selected a mode: Ask for headless, [`default_interactive_permission_mode`] for the TUI.
pub fn effective_auto_for_launch(
    cli_always_approve: bool,
    cli_permission_mode: Option<&str>,
    remote_permission_mode: Option<&str>,
    unset_default: PermissionMode,
) -> bool {
    // Feature gate (default ON): when the auto permission-mode feature is disabled, Auto is inert regardless of CLI/config
    // Never launching into auto means the classifier never wires. See `resolve_auto_permission_mode_enabled`.
    if !crate::util::config::auto_permission_mode_enabled_from_disk() {
        return false;
    }
    // Explicit --yolo without a competing --permission-mode is not auto
    if cli_always_approve && cli_permission_mode.is_none() {
        return false;
    }
    let yolo = effective_yolo_for_launch(
        cli_always_approve,
        cli_permission_mode,
        remote_permission_mode,
    );
    if yolo.yolo {
        return false;
    }
    // --yolo plus --permission-mode auto: prefer yolo only when mode is full bypass
    if cli_always_approve && matches!(cli_permission_mode, Some("auto")) {
        return false;
    }
    if let Some(mode) = cli_permission_mode {
        return mode == "auto";
    }
    load_selected_permission_mode(remote_permission_mode)
        .unwrap_or(unset_default)
        .is_auto()
}

/// Auto can be requested via CLI, config, `default_auto_mode`, or a client's `_meta.autoMode`.
/// It is pure so both activation call sites (session spawn and runtime `SetAutoMode`) are unit-testable without a live session.
/// This is the authoritative agent-side gate: when it returns `false`, the permission manager never flips to auto and the classifier never wires.
pub(crate) fn auto_mode_session_active(
    gate_enabled: bool,
    requested_auto: bool,
    session_yolo: bool,
) -> bool {
    gate_enabled && requested_auto && !session_yolo
}

/// The precedence logic, kept pure so tests can call it directly.
fn resolve_effective_yolo(
    cli_always_approve: bool,
    cli_permission_mode: Option<&str>,
    config_is_always_approve: bool,
) -> bool {
    if let Some(mode) = cli_permission_mode {
        // Only the two "always approve everything" variants produce YOLO.
        matches!(mode, "bypassPermissions" | "always-approve")
    } else if cli_always_approve {
        true
    } else {
        config_is_always_approve
    }
}

/// Pure: combines the requested bypass with the policy pin.
fn resolve_launch_yolo(requested: bool, policy_block: Option<&'static str>) -> EffectiveYolo {
    EffectiveYolo {
        yolo: requested && policy_block.is_none(),
        blocked_warning: if requested { policy_block } else { None },
        policy_block,
    }
}

use distill_workspace::permission::resolution::yolo_disabled_by_policy;

fn require_plan_approval_from_layers(layers: &crate::config::ConfigLayers) -> bool {
    layers
        .effective_config_base_without_overlay()
        .as_table()
        .and_then(|t| t.get("ui"))
        .and_then(|v| v.as_table())
        .and_then(|ui| ui.get("require_plan_approval"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Load `[ui] require_plan_approval` from the merged config layers (overlay-free).
/// When `true`, the plan viewer always opens for explicit user approval when the agent calls `exit_plan_mode`, even in always-approve (YOLO) mode.
pub fn load_require_plan_approval() -> bool {
    let layers = match crate::config::ConfigLayers::load() {
        Ok(l) => l,
        Err(_) => return false,
    };
    require_plan_approval_from_layers(&layers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_permission_mode_none_is_ask() {
        assert_eq!(resolve_permission_mode(None, None), PermissionMode::Ask);
    }

    #[test]
    fn campaign_cannot_arm_permission_mode_gate() {
        let mut layers = crate::config::ConfigLayers::default();
        layers.campaigns.managed = vec![distill_config::CampaignEntry {
            id: "c1".into(),
            patch: toml::from_str("[ui]\npermission_mode = \"always-approve\"\n").unwrap(),
        }];
        assert_eq!(
            permission_mode_from_layers(&layers, None),
            PermissionMode::Ask
        );
    }

    #[test]
    fn resolve_permission_mode_remote_only() {
        assert_eq!(
            resolve_permission_mode(None, Some("auto")),
            PermissionMode::Auto,
        );
        assert_eq!(
            resolve_permission_mode(None, Some("always-approve")),
            PermissionMode::AlwaysApprove,
        );
        assert_eq!(
            resolve_permission_mode(None, Some("ask")),
            PermissionMode::Ask,
        );
        assert_eq!(
            resolve_permission_mode(None, Some("default")),
            PermissionMode::Ask,
        );
    }

    #[test]
    fn resolve_permission_mode_toml_wins_over_remote() {
        let root: TomlValue = toml::from_str("[ui]\npermission_mode = \"ask\"\n").unwrap();
        assert_eq!(
            resolve_permission_mode(Some(root.get("ui").unwrap()), Some("always-approve")),
            PermissionMode::Ask,
        );
        let yolo: TomlValue = toml::from_str("[ui]\nyolo = true\n").unwrap();
        assert_eq!(
            resolve_permission_mode(Some(yolo.get("ui").unwrap()), Some("ask")),
            PermissionMode::AlwaysApprove,
        );
        let yolo_off: TomlValue = toml::from_str("[ui]\nyolo = false\n").unwrap();
        assert_eq!(
            resolve_permission_mode(Some(yolo_off.get("ui").unwrap()), Some("always-approve")),
            PermissionMode::Ask,
        );
        let approval: TomlValue = toml::from_str("[ui]\napproval_mode = \"ask\"\n").unwrap();
        assert_eq!(
            resolve_permission_mode(Some(approval.get("ui").unwrap()), Some("auto")),
            PermissionMode::Ask,
        );
    }

    #[test]
    fn permission_mode_from_ui_if_set_none_when_no_keys() {
        let theme: TomlValue = toml::from_str("[ui]\ntheme = \"groknight\"\n").unwrap();
        assert_eq!(
            permission_mode_from_ui_if_set(theme.get("ui").unwrap()),
            None,
        );
        assert_eq!(
            permission_mode_from_ui_if_set(&TomlValue::String("nope".into())),
            None,
        );
        let yolo_off: TomlValue = toml::from_str("[ui]\nyolo = false\n").unwrap();
        assert_eq!(
            permission_mode_from_ui_if_set(yolo_off.get("ui").unwrap()),
            Some(PermissionMode::Ask),
        );
    }

    #[test]
    fn resolve_permission_mode_unknown_remote_is_ask() {
        assert_eq!(
            resolve_permission_mode(None, Some("garbage")),
            PermissionMode::Ask,
        );
        assert_eq!(resolve_permission_mode(None, Some("")), PermissionMode::Ask);
    }

    #[test]
    fn parse_permission_mode_canonical_covers_all_canonicals_plus_fallback() {
        assert_eq!(
            parse_permission_mode_canonical("always-approve"),
            PermissionMode::AlwaysApprove,
        );
        assert_eq!(
            parse_permission_mode_canonical("auto"),
            PermissionMode::Auto,
        );
        assert_eq!(parse_permission_mode_canonical("ask"), PermissionMode::Ask,);
        assert_eq!(
            parse_permission_mode_canonical("default"),
            PermissionMode::Ask,
            "PR 11: 'default' canonical projects onto Ask at the runtime layer; \
             a future enum extension would change this arm",
        );
        // Unknown or corrupt strings fall back to Ask (safer direction, no YOLO bypass)
        assert_eq!(
            parse_permission_mode_canonical("garbage"),
            PermissionMode::Ask,
        );
        assert_eq!(parse_permission_mode_canonical(""), PermissionMode::Ask,);
        assert_eq!(
            parse_permission_mode_canonical("Always-Approve"),
            PermissionMode::Ask,
            "wire format is case-sensitive; 'Always-Approve' is unknown",
        );
    }

    /// `resolve_permission_mode` is the pure logic `load_permission_mode` delegates to.
    #[test]
    fn resolve_permission_mode_ui_precedence_and_canonicalization() {
        let cases: &[(&str, PermissionMode, &str)] = &[
            // Primary key, canonicalized.
            (
                "[ui]\npermission_mode = \"always-approve\"\n",
                PermissionMode::AlwaysApprove,
                "always-approve",
            ),
            (
                "[ui]\npermission_mode = \"auto\"\n",
                PermissionMode::Auto,
                "auto",
            ),
            (
                "[ui]\npermission_mode = \"default\"\n",
                PermissionMode::Ask,
                "ask",
            ),
            (
                "[ui]\npermission_mode = \"garbage\"\n",
                PermissionMode::Ask,
                "ask",
            ),
            // Legacy keys.
            (
                "[ui]\napproval_mode = \"always-approve\"\n",
                PermissionMode::AlwaysApprove,
                "always-approve",
            ),
            (
                "[ui]\napproval_mode = \"ask\"\n",
                PermissionMode::Ask,
                "ask",
            ),
            (
                "[ui]\nyolo = true\n",
                PermissionMode::AlwaysApprove,
                "always-approve",
            ),
            ("[ui]\nyolo = false\n", PermissionMode::Ask, "ask"),
            // Precedence: permission_mode wins over legacy keys.
            (
                "[ui]\npermission_mode = \"ask\"\nyolo = true\napproval_mode = \"always-approve\"\n",
                PermissionMode::Ask,
                "ask",
            ),
            // approval_mode wins over yolo.
            (
                "[ui]\napproval_mode = \"ask\"\nyolo = true\n",
                PermissionMode::Ask,
                "ask",
            ),
            // No permission keys fall back to Ask
            ("[ui]\ntheme = \"groknight\"\n", PermissionMode::Ask, "ask"),
        ];
        for (toml_str, expected_mode, expected_canonical) in cases {
            let root: TomlValue = toml::from_str(toml_str).unwrap();
            let ui = root.get("ui").expect("test config defines [ui]");
            let mode = resolve_permission_mode(Some(ui), None);
            assert_eq!(mode, *expected_mode, "config {toml_str:?}");
            assert_eq!(
                permission_mode_canonical_str(mode),
                *expected_canonical,
                "config {toml_str:?} canonical string",
            );
        }
        // A non-table [ui] value resolves to Ask (defensive).
        assert_eq!(
            resolve_permission_mode(Some(&TomlValue::String("nope".into())), None),
            PermissionMode::Ask,
        );
    }

    #[test]
    fn resolve_effective_yolo_precedence_is_correct() {
        use super::resolve_effective_yolo;

        // Table-driven: (cli_yolo, cli_perm_mode, config_yolo, expected_yolo, description)
        let cases: &[(bool, Option<&str>, bool, bool, &str)] = &[
            // --- CLI --permission-mode present: it wins completely ---
            (
                false,
                Some("plan"),
                true,
                false,
                "plan + config yolo → false",
            ),
            (
                false,
                Some("plan"),
                false,
                false,
                "plan + config safe → false",
            ),
            (
                true,
                Some("plan"),
                true,
                false,
                "plan beats even explicit --yolo",
            ),
            (
                false,
                Some("dontAsk"),
                true,
                false,
                "dontAsk forces no auto-approve",
            ),
            (
                false,
                Some("default"),
                true,
                false,
                "default forces no auto-approve",
            ),
            (
                false,
                Some("acceptEdits"),
                true,
                false,
                "acceptEdits is not full yolo",
            ),
            (false, Some("auto"), true, false, "auto is not full yolo"),
            (
                false,
                Some("bypassPermissions"),
                false,
                true,
                "bypassPermissions → yolo",
            ),
            (
                false,
                Some("always-approve"),
                false,
                true,
                "legacy always-approve string → yolo",
            ),
            (
                false,
                Some("garbage"),
                true,
                false,
                "unknown mode is safe (no yolo)",
            ),
            (false, Some(""), true, false, "empty mode string is safe"),
            (
                true,
                Some("bypassPermissions"),
                false,
                true,
                "bypass + --yolo still yolo",
            ),
            // --- No --permission-mode: fall back to legacy --yolo then config ---
            (true, None, false, true, "--yolo alone → yolo"),
            (true, None, true, true, "--yolo + config yolo → yolo"),
            (false, None, true, true, "no cli flags + config yolo → yolo"),
            (
                false,
                None,
                false,
                false,
                "no cli flags + config safe → safe",
            ),
        ];

        for &(cli_yolo, perm, cfg_yolo, expected, desc) in cases {
            let actual = resolve_effective_yolo(cli_yolo, perm, cfg_yolo);
            assert_eq!(
                actual, expected,
                "failed case: {desc} (cli_yolo={cli_yolo}, perm={perm:?}, cfg_yolo={cfg_yolo})"
            );
        }
    }

    #[test]
    fn effective_yolo_for_launch_wrapper_calls_resolve() {
        // Cover the deterministic CLI precedence paths only; the pure-config fallback isn't controllable here
        // Pin composition is proven by `resolve_launch_yolo_policy_pin_neutralizes_requested_bypass`
        // Comparing the wrapper against `yolo_disabled_by_policy()` would pass even if the wrapper dropped the pin, so that check is omitted
        assert!(!effective_yolo_for_launch(false, Some("plan"), None).yolo);
        assert!(!effective_yolo_for_launch(false, Some("dontAsk"), None).yolo);
    }

    /// The dangerous row (remote always-approve must never override an explicit CLI ask) is deterministic on any host.
    /// The positive row is skipped under a host requirements pin.
    /// Pin composition is proven separately by `resolve_launch_yolo_policy_pin_neutralizes_requested_bypass`.
    #[test]
    fn effective_yolo_for_launch_cli_beats_remote() {
        assert!(
            !effective_yolo_for_launch(false, Some("ask"), Some("always-approve")).yolo,
            "remote always-approve must not override CLI --permission-mode ask"
        );
        if yolo_disabled_by_policy().is_none() {
            assert!(
                effective_yolo_for_launch(true, None, Some("ask")).yolo,
                "remote ask must not override CLI --yolo"
            );
        }
    }

    /// Display clamp: modes that lost enforcement (policy-pinned AlwaysApprove, gated-off Auto) show Ask.
    /// The persisted TOML `"default"` spelling survives as its own visible option.
    #[test]
    fn resolved_display_permission_mode_clamps_and_preserves_default() {
        assert_eq!(
            clamped_display_permission_mode(PermissionMode::AlwaysApprove),
            "ask"
        );
        assert_eq!(clamped_display_permission_mode(PermissionMode::Auto), "ask");
        assert_eq!(clamped_display_permission_mode(PermissionMode::Ask), "ask");

        let default_ui: TomlValue =
            toml::from_str("[ui]\npermission_mode = \"default\"\n").unwrap();
        assert_eq!(
            resolved_display_permission_mode(default_ui.get("ui"), Some("always-approve")),
            "default",
            "persisted 'default' must not collapse onto 'ask' for display"
        );
        assert_eq!(resolved_display_permission_mode(None, Some("auto")), "ask");
        assert_eq!(resolved_display_permission_mode(None, None), "ask");
    }

    #[test]
    fn effective_auto_for_launch_cli_auto_not_yolo() {
        // This function is feature-gated; force the gate ON (and serialize with the other env-sensitive gate tests) so the auto-activation paths run
        let _g = crate::util::config::resolve::AUTO_PERMISSION_MODE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::set_var("GROK_AUTO_PERMISSION_MODE", "1") };
        assert!(effective_auto_for_launch(
            false,
            Some("auto"),
            None,
            PermissionMode::Ask
        ));
        assert!(
            !effective_auto_for_launch(true, Some("auto"), None, PermissionMode::Ask),
            "--yolo beats auto"
        );
        assert!(!effective_auto_for_launch(
            false,
            Some("always-approve"),
            None,
            PermissionMode::Ask
        ));
        assert!(!effective_auto_for_launch(
            false,
            Some("ask"),
            None,
            PermissionMode::Ask
        ));
        unsafe { std::env::remove_var("GROK_AUTO_PERMISSION_MODE") };
    }

    /// The authoritative agent-side gate (used at the `set_auto_mode` call site).
    /// Gate OFF must never activate, even with a client `_meta.autoMode=true` (the `requested_auto=true` case).
    #[test]
    fn auto_mode_session_active_requires_gate_request_and_no_yolo() {
        assert!(
            !auto_mode_session_active(false, true, false),
            "gate OFF must not activate auto even when requested"
        );
        assert!(
            auto_mode_session_active(true, true, false),
            "gate ON + requested + no yolo activates auto"
        );
        assert!(
            !auto_mode_session_active(true, true, true),
            "yolo wins over auto"
        );
        assert!(
            !auto_mode_session_active(true, false, false),
            "not requested ⇒ inactive"
        );
    }

    /// With the gate forced OFF (`GROK_AUTO_PERMISSION_MODE=0`), `--permission-mode auto` or config auto is inert, so the classifier never launches.
    /// (Compiled-in default is ON; this pins the env kill-switch.)
    #[test]
    fn effective_auto_for_launch_inert_when_gate_off() {
        let _g = crate::util::config::resolve::AUTO_PERMISSION_MODE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::set_var("GROK_AUTO_PERMISSION_MODE", "0") };
        assert!(
            !effective_auto_for_launch(false, Some("auto"), None, PermissionMode::Ask),
            "gate OFF: explicit --permission-mode auto must not activate auto"
        );
        assert!(
            !effective_auto_for_launch(false, None, None, PermissionMode::Ask),
            "gate OFF: config-driven auto must not activate auto"
        );
        assert!(
            !effective_auto_for_launch(false, None, None, PermissionMode::Auto),
            "gate OFF: an Auto unset-default must be inert"
        );
        unsafe { std::env::remove_var("GROK_AUTO_PERMISSION_MODE") };
    }

    #[test]
    fn interactive_default_slot() {
        let _g = crate::util::config::resolve::AUTO_PERMISSION_MODE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::remove_var(ENV_DEFAULT_PERMISSION_MODE) };
        assert_eq!(
            default_interactive_permission_mode(),
            DEFAULT_INTERACTIVE_PERMISSION_MODE,
        );
        for (raw, expected) in [
            ("auto", PermissionMode::Auto),
            ("ask", PermissionMode::Ask),
            ("default", PermissionMode::Ask),
            ("always-approve", DEFAULT_INTERACTIVE_PERMISSION_MODE),
            ("garbage", DEFAULT_INTERACTIVE_PERMISSION_MODE),
        ] {
            unsafe { std::env::set_var(ENV_DEFAULT_PERMISSION_MODE, raw) };
            assert_eq!(
                default_interactive_permission_mode(),
                expected,
                "GROK_DEFAULT_PERMISSION_MODE={raw}"
            );
        }

        assert_eq!(selected_permission_mode(None, None), None);
        let no_perm_keys: TomlValue = toml::from_str("[ui]\ntheme = \"dark\"\n").unwrap();
        assert_eq!(selected_permission_mode(no_perm_keys.get("ui"), None), None);
        assert_eq!(
            selected_permission_mode(None, Some("garbage")),
            Some(PermissionMode::Ask),
        );

        unsafe { std::env::set_var("GROK_AUTO_PERMISSION_MODE", "1") };
        unsafe { std::env::set_var(ENV_DEFAULT_PERMISSION_MODE, "auto") };
        let unset = default_interactive_permission_mode();
        assert!(!effective_auto_for_launch(false, Some("ask"), None, unset));
        assert!(!effective_auto_for_launch(true, None, None, unset));
        unsafe { std::env::remove_var(ENV_DEFAULT_PERMISSION_MODE) };
        unsafe { std::env::remove_var("GROK_AUTO_PERMISSION_MODE") };
    }

    // Pure tests for the policy predicate itself live next to its canonical definition in `distill_workspace::permission::resolution`

    #[test]
    fn resolve_launch_yolo_policy_pin_neutralizes_requested_bypass() {
        let warning =
            distill_workspace::permission::resolution::YoloPinReason::DisableBypassPermissionsMode
                .message();
        // A pin with a requested bypass forces yolo off and carries a warning to show
        assert_eq!(
            resolve_launch_yolo(true, Some(warning)),
            EffectiveYolo {
                yolo: false,
                blocked_warning: Some(warning),
                policy_block: Some(warning),
            },
        );
        // A pin without a requested bypass is off and silent; the pin is still carried
        assert_eq!(
            resolve_launch_yolo(false, Some(warning)),
            EffectiveYolo {
                yolo: false,
                blocked_warning: None,
                policy_block: Some(warning),
            },
        );
        // With no pin the requested value passes through unchanged
        assert_eq!(
            resolve_launch_yolo(true, None),
            EffectiveYolo {
                yolo: true,
                blocked_warning: None,
                policy_block: None,
            },
        );
        assert_eq!(
            resolve_launch_yolo(false, None),
            EffectiveYolo {
                yolo: false,
                blocked_warning: None,
                policy_block: None,
            },
        );
    }

    fn ui_layer(body: &str) -> TomlValue {
        toml::from_str(&format!("[ui]\n{body}\n")).unwrap()
    }

    #[test]
    fn env_overlay_cannot_escalate_security_gates() {
        let user_opt_out = crate::config::ConfigLayers {
            user: ui_layer("permission_mode = \"ask\"\nrequire_plan_approval = true"),
            env_overlay: Some(ui_layer(
                "permission_mode = \"always-approve\"\nyolo = true\nrequire_plan_approval = false",
            )),
            ..Default::default()
        };
        assert_eq!(
            permission_mode_from_layers(&user_opt_out, None),
            PermissionMode::Ask,
        );
        assert!(require_plan_approval_from_layers(&user_opt_out));

        let managed_opt_out = crate::config::ConfigLayers {
            managed: ui_layer("permission_mode = \"ask\""),
            env_overlay: Some(ui_layer("permission_mode = \"always-approve\"")),
            ..Default::default()
        };
        assert_eq!(
            permission_mode_from_layers(&managed_opt_out, None),
            PermissionMode::Ask,
        );

        let requirements_clamp = crate::config::ConfigLayers {
            user: ui_layer("permission_mode = \"always-approve\""),
            env_overlay: Some(ui_layer("permission_mode = \"always-approve\"")),
            user_requirements: Some(ui_layer("permission_mode = \"ask\"")),
            ..Default::default()
        };
        assert_eq!(
            permission_mode_from_layers(&requirements_clamp, None),
            PermissionMode::Ask,
        );
    }
}
