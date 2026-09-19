// Modified for Distill by Samuel Fajreldines, 2026.
//! Single home for everything that decides which row the approval menu highlights when the agent asks for permission:
//!
//! - [`DefaultSelectedPermission`], the value type (maps to and from config strings and ACP kinds),
//! - the process-wide caches (the configured value and the sticky last-used kind),
//! - [`resolve_initial_cursor`], the one function the ACP handler calls when queueing a prompt.
//!
//! This is deliberately not in `views::permission_view` (a renderer) or `appearance::cache` (generic bool/u8 setting caches).
//! The type and its state live together here, and the cache module (hot in the render path) does not depend upward on the view layer.

use std::cell::Cell;

use agent_client_protocol as acp;
use distill_workspace::permission::is_enable_always_approve_option;

/// Persisted as `[ui].default_selected_permission`. Steers only the first prompt; later prompts stick to the last-used kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSelectedPermission {
    /// The global "Always allow on all sessions" (enable-always-approve) row.
    /// It is also the fallback for an unset / unrecognised config value, so it is the effective default.
    AlwaysAllowAllSessions,
    AllowOnce,
    /// The prompt-scoped always-allow row ("Always allow this command" /
    /// tool / domain / edit-session).
    AllowCommandAlways,
    Reject,
}

impl DefaultSelectedPermission {
    /// The single canonical config.toml / registry string for this variant.
    /// It is `const` so the settings catalog can build its choice table at compile time.
    pub const fn as_canonical(self) -> &'static str {
        match self {
            Self::AlwaysAllowAllSessions => "always_allow_all_sessions",
            Self::AllowOnce => "allow_once",
            Self::AllowCommandAlways => "allow_command_always",
            Self::Reject => "reject",
        }
    }

    /// Display label for the settings picker and the change toast.
    /// `AllowCommandAlways` preselects the prompt-specific always-allow row (per-command / per-tool / per-domain / per-edit-session).
    /// The global allow-everything row belongs to `AlwaysAllowAllSessions`.
    pub const fn display(self) -> &'static str {
        match self {
            Self::AlwaysAllowAllSessions => "Always allow on all sessions",
            Self::AllowOnce => "Allow once",
            Self::AllowCommandAlways => "Always allow this command",
            Self::Reject => "Reject",
        }
    }

    /// Parse a config.toml / registry value (trimmed, case-insensitive, no aliases).
    /// Both `always_allow_all_sessions` and any unrecognised or empty value resolve to [`AlwaysAllowAllSessions`](Self::AlwaysAllowAllSessions).
    /// No `Option` has to be threaded through callers.
    pub fn from_config_value(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow_once" => Self::AllowOnce,
            "allow_command_always" => Self::AllowCommandAlways,
            "reject" => Self::Reject,
            _ => Self::AlwaysAllowAllSessions,
        }
    }

    /// [`AlwaysAllowAllSessions`](Self::AlwaysAllowAllSessions) matches by row identity, not kind.
    /// [`Reject`](Self::Reject) matches both reject kinds so a sticky reject lands on whichever reject row is offered.
    pub fn matches_kind(self, kind: &acp::PermissionOptionKind) -> bool {
        matches!(
            (self, kind),
            (Self::AllowOnce, acp::PermissionOptionKind::AllowOnce)
                | (
                    Self::AllowCommandAlways,
                    acp::PermissionOptionKind::AllowAlways
                )
                | (
                    Self::Reject,
                    acp::PermissionOptionKind::RejectOnce | acp::PermissionOptionKind::RejectAlways
                )
        )
    }

    /// Both reject kinds collapse to [`Reject`](Self::Reject). Never yields [`AlwaysAllowAllSessions`](Self::AlwaysAllowAllSessions); that row is excluded from sticky recording.
    pub fn from_kind(kind: &acp::PermissionOptionKind) -> Self {
        match kind {
            acp::PermissionOptionKind::AllowOnce => Self::AllowOnce,
            acp::PermissionOptionKind::AllowAlways => Self::AllowCommandAlways,
            acp::PermissionOptionKind::RejectOnce | acp::PermissionOptionKind::RejectAlways => {
                Self::Reject
            }
            // TODO(acp-0.10): `PermissionOptionKind` is #[non_exhaustive];
            // treat unknown kinds as reject (never auto-allow).
            _ => Self::Reject,
        }
    }
}

// First-prompt config cache. Seeded at startup so mid-session reads never hit disk.
// `AlwaysAllowAllSessions` is the unset default.

thread_local! {
    static CONFIG_CURRENT: Cell<DefaultSelectedPermission> =
        const { Cell::new(DefaultSelectedPermission::AlwaysAllowAllSessions) };
    static CONFIG_LOADED: Cell<bool> = const { Cell::new(false) };
}

/// Env `GROK_DEFAULT_SELECTED_PERMISSION`, then `[ui].default_selected_permission`, then AlwaysAllowAllSessions.
/// Empty or unrecognised values fall through. Env exists so tests override without editing config.
pub fn load_default_selected_permission() -> DefaultSelectedPermission {
    CONFIG_LOADED.with(|loaded| {
        if !loaded.get() {
            let resolved = std::env::var("GROK_DEFAULT_SELECTED_PERMISSION")
                .ok()
                .map(|s| DefaultSelectedPermission::from_config_value(&s))
                .filter(|p| *p != DefaultSelectedPermission::AlwaysAllowAllSessions)
                .or_else(|| {
                    load_string_from_effective_config("default_selected_permission")
                        .map(|s| DefaultSelectedPermission::from_config_value(&s))
                })
                .unwrap_or(DefaultSelectedPermission::AlwaysAllowAllSessions);
            CONFIG_CURRENT.with(|c| c.set(resolved));
            loaded.set(true);
        }
    });
    CONFIG_CURRENT.with(Cell::get)
}

/// Replace the cached configured value (optimistic update from the settings modal, or rollback on persist failure).
/// The next prompt sees it without a restart.
pub fn set_default_selected_permission(value: DefaultSelectedPermission) {
    CONFIG_CURRENT.with(|c| c.set(value));
    CONFIG_LOADED.with(|l| l.set(true));
}

/// Eagerly seed the configured-value cache at startup (called by `appearance::cache::prime`) so the first prompt never hits disk.
pub fn prime() {
    let _ = load_default_selected_permission();
}

// Ephemeral last-confirmed kind; after the first prompt it beats the configured value.
// `AlwaysAllowAllSessions` means nothing confirmed yet. Single-threaded TUI, so a thread-local Cell is enough.

thread_local! {
    static LAST_USED: Cell<DefaultSelectedPermission> =
        const { Cell::new(DefaultSelectedPermission::AlwaysAllowAllSessions) };
}

/// The kind the user last confirmed this session.
/// Falls back to the [`AlwaysAllowAllSessions`](DefaultSelectedPermission::AlwaysAllowAllSessions) sentinel if none yet.
pub fn last_used_permission() -> DefaultSelectedPermission {
    LAST_USED.with(Cell::get)
}

/// Record the kind the user just confirmed.
/// Callers must skip the special enable-always-approve (YOLO) and allow-edits-session options.
/// Neither represents a per-prompt choice that should steer a later prompt's cursor.
pub fn set_last_used_permission(kind: DefaultSelectedPermission) {
    LAST_USED.with(|c| c.set(kind));
}

// ── Resolution ──────────────────────────────────────────────────────────────

/// Sticky last-used, then configured default, then the YOLO row by identity, else index 0.
/// A concrete target skips YOLO. A missing target kind degrades to one-shot allow, never to global always-approve.
pub fn resolve_initial_cursor(options: &[acp::PermissionOption]) -> usize {
    let target = match last_used_permission() {
        DefaultSelectedPermission::AlwaysAllowAllSessions => load_default_selected_permission(),
        sticky => sticky,
    };
    if let Some(idx) = options
        .iter()
        .position(|o| target.matches_kind(&o.kind) && !is_enable_always_approve_option(o))
    {
        return idx;
    }
    if target != DefaultSelectedPermission::AlwaysAllowAllSessions
        && let Some(idx) = options.iter().position(|o| {
            o.kind == acp::PermissionOptionKind::AllowOnce && !is_enable_always_approve_option(o)
        })
    {
        return idx;
    }
    options
        .iter()
        .position(is_enable_always_approve_option)
        .unwrap_or(0)
}

/// Read a `[ui].<key>` string from the shell's layered effective config.
/// Returns `None` when the key is absent or not a string.
fn load_string_from_effective_config(key: &str) -> Option<String> {
    let root = distill_config::load_effective_config_disk_only().ok()?;
    root.get("ui")?
        .get(key)?
        .as_str()
        .map(std::string::ToString::to_string)
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use distill_workspace::permission::ENABLE_ALWAYS_APPROVE_OPTION_ID;

    fn opt(id: &str, kind: acp::PermissionOptionKind) -> acp::PermissionOption {
        acp::PermissionOption::new(acp::PermissionOptionId::new(id), id.to_owned(), kind)
    }

    #[test]
    fn from_config_value_accepts_one_canonical_per_variant() {
        // Trimmed and case-insensitive, but still the exact canonical token
        assert_eq!(
            DefaultSelectedPermission::from_config_value("allow_once"),
            DefaultSelectedPermission::AllowOnce
        );
        assert_eq!(
            DefaultSelectedPermission::from_config_value("  ALLOW_COMMAND_ALWAYS  "),
            DefaultSelectedPermission::AllowCommandAlways
        );
        assert_eq!(
            DefaultSelectedPermission::from_config_value("reject"),
            DefaultSelectedPermission::Reject
        );
        assert_eq!(
            DefaultSelectedPermission::from_config_value("always_allow_all_sessions"),
            DefaultSelectedPermission::AlwaysAllowAllSessions
        );
        // Empty and garbage collapse to the effective default.
        assert_eq!(
            DefaultSelectedPermission::from_config_value(""),
            DefaultSelectedPermission::AlwaysAllowAllSessions
        );
        assert_eq!(
            DefaultSelectedPermission::from_config_value("bogus"),
            DefaultSelectedPermission::AlwaysAllowAllSessions
        );
    }

    #[test]
    fn canonical_round_trips() {
        // The enum is the single source of truth for the strings
        for variant in [
            DefaultSelectedPermission::AlwaysAllowAllSessions,
            DefaultSelectedPermission::AllowOnce,
            DefaultSelectedPermission::AllowCommandAlways,
            DefaultSelectedPermission::Reject,
        ] {
            assert_eq!(
                DefaultSelectedPermission::from_config_value(variant.as_canonical()),
                variant,
            );
            assert!(!variant.display().is_empty());
        }
    }

    #[test]
    fn matches_kind_targets_the_right_rows() {
        use DefaultSelectedPermission as P;
        assert!(P::AllowOnce.matches_kind(&acp::PermissionOptionKind::AllowOnce));
        assert!(P::AllowCommandAlways.matches_kind(&acp::PermissionOptionKind::AllowAlways));
        assert!(!P::AllowCommandAlways.matches_kind(&acp::PermissionOptionKind::AllowOnce));
        // A sticky reject must land on EITHER reject row, since some prompts only offer `RejectAlways`
        assert!(P::Reject.matches_kind(&acp::PermissionOptionKind::RejectOnce));
        assert!(P::Reject.matches_kind(&acp::PermissionOptionKind::RejectAlways));
        // The always-allow-on-all-sessions sentinel targets no specific kind (its row is matched by identity; the cursor falls to it)
        for kind in [
            acp::PermissionOptionKind::AllowOnce,
            acp::PermissionOptionKind::AllowAlways,
            acp::PermissionOptionKind::RejectOnce,
            acp::PermissionOptionKind::RejectAlways,
        ] {
            assert!(!P::AlwaysAllowAllSessions.matches_kind(&kind));
        }
    }

    #[test]
    fn from_kind_is_total_and_collapses_reject() {
        assert_eq!(
            DefaultSelectedPermission::from_kind(&acp::PermissionOptionKind::AllowOnce),
            DefaultSelectedPermission::AllowOnce
        );
        assert_eq!(
            DefaultSelectedPermission::from_kind(&acp::PermissionOptionKind::AllowAlways),
            DefaultSelectedPermission::AllowCommandAlways
        );
        assert_eq!(
            DefaultSelectedPermission::from_kind(&acp::PermissionOptionKind::RejectOnce),
            DefaultSelectedPermission::Reject
        );
        // `RejectAlways` ("No, and don't run X") collapses to Reject so the next prompt still lands on its reject row
        assert_eq!(
            DefaultSelectedPermission::from_kind(&acp::PermissionOptionKind::RejectAlways),
            DefaultSelectedPermission::Reject
        );
    }

    #[test]
    fn last_used_round_trips() {
        // A fresh thread has an un-seeded thread-local (it starts at the sentinel)
        std::thread::spawn(|| {
            assert_eq!(
                last_used_permission(),
                DefaultSelectedPermission::AlwaysAllowAllSessions
            );
            set_last_used_permission(DefaultSelectedPermission::AllowCommandAlways);
            assert_eq!(
                last_used_permission(),
                DefaultSelectedPermission::AllowCommandAlways
            );
            set_last_used_permission(DefaultSelectedPermission::Reject);
            assert_eq!(last_used_permission(), DefaultSelectedPermission::Reject);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn resolve_unset_lands_on_yolo_row() {
        std::thread::spawn(|| {
            // Force the config cache to the default without touching disk/env.
            set_default_selected_permission(DefaultSelectedPermission::AlwaysAllowAllSessions);
            let options = [
                opt("allow-once", acp::PermissionOptionKind::AllowOnce),
                opt(
                    ENABLE_ALWAYS_APPROVE_OPTION_ID,
                    acp::PermissionOptionKind::AllowOnce,
                ),
                opt("reject-once", acp::PermissionOptionKind::RejectOnce),
            ];
            // With no sticky kind and the default config, the cursor lands on the enable-always-approve row
            // It is matched by identity (index 1), not the first AllowOnce (index 0)
            assert_eq!(resolve_initial_cursor(&options), 1);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn resolve_sticky_skips_yolo_row() {
        std::thread::spawn(|| {
            set_default_selected_permission(DefaultSelectedPermission::AlwaysAllowAllSessions);
            set_last_used_permission(DefaultSelectedPermission::AllowOnce);
            let options = [
                opt(
                    ENABLE_ALWAYS_APPROVE_OPTION_ID,
                    acp::PermissionOptionKind::AllowOnce,
                ),
                opt("allow-once", acp::PermissionOptionKind::AllowOnce),
                opt("reject-once", acp::PermissionOptionKind::RejectOnce),
            ];
            // Sticky AllowOnce must skip the YOLO row (also AllowOnce kind) and land on the plain allow-once row (index 1)
            assert_eq!(resolve_initial_cursor(&options), 1);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn resolve_sticky_reject_lands_on_reject_always_only_prompt() {
        std::thread::spawn(|| {
            set_default_selected_permission(DefaultSelectedPermission::AlwaysAllowAllSessions);
            set_last_used_permission(DefaultSelectedPermission::Reject);
            let options = [
                opt(
                    ENABLE_ALWAYS_APPROVE_OPTION_ID,
                    acp::PermissionOptionKind::AllowOnce,
                ),
                opt("allow-once", acp::PermissionOptionKind::AllowOnce),
                opt("reject-always", acp::PermissionOptionKind::RejectAlways),
            ];
            // The prompt offers only `RejectAlways`; a sticky reject must still find it (index 2) rather than falling back to the YOLO row
            assert_eq!(resolve_initial_cursor(&options), 2);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn resolve_without_yolo_row_falls_back_to_index_0() {
        std::thread::spawn(|| {
            set_default_selected_permission(DefaultSelectedPermission::AlwaysAllowAllSessions);
            let options = [
                opt("allow-once", acp::PermissionOptionKind::AllowOnce),
                opt("reject-once", acp::PermissionOptionKind::RejectOnce),
            ];
            // No sticky kind, default config, no YOLO row (non-TUI client), so the cursor falls back to index 0
            assert_eq!(resolve_initial_cursor(&options), 0);
        })
        .join()
        .unwrap();
    }

    /// A concrete sticky target with no matching row (the always-allow row may be suppressed as unhonorable) degrades to the one-shot allow.
    /// It never escalates to the global always-approve row, which is one Enter away from persisting always-approve mode.
    #[test]
    fn unmatched_concrete_target_degrades_to_allow_once_not_yolo() {
        std::thread::spawn(|| {
            set_last_used_permission(DefaultSelectedPermission::AllowCommandAlways);
            let options = [
                opt(
                    distill_workspace::permission::ENABLE_ALWAYS_APPROVE_OPTION_ID,
                    acp::PermissionOptionKind::AllowOnce,
                ),
                opt("allow-once", acp::PermissionOptionKind::AllowOnce),
                opt("reject-once", acp::PermissionOptionKind::RejectOnce),
                opt(
                    "reject-always-command",
                    acp::PermissionOptionKind::RejectAlways,
                ),
            ];
            assert_eq!(
                resolve_initial_cursor(&options),
                1,
                "cursor must land on the plain allow-once row"
            );
        })
        .join()
        .unwrap();
    }
}
