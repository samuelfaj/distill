// Modified for Distill by Samuel Fajreldines, 2026.
//! Managed MCP + plugin + marketplace policy engine: `managed-settings.json`
//! plus every `managed_config.toml` / `requirements.toml` layer, resolved
//! strictest-wins into MCP/marketplace allowlists and tighten-only pins.
//!
//! Extracted move-only from `permission::resolution`, which re-exports the
//! public surface so existing `resolution::` paths keep working.

mod layer;
mod marketplace;
mod mcp;
mod parse;
mod url_match;
mod verdict;

pub use layer::{PolicyLayerOwnership, PolicyPin, PolicySourceAuthority};
pub use marketplace::{
    ManagedMarketplace, ManagedMarketplaceKind, MarketplaceAllowlist, MarketplacePolicy,
    normalize_git_url,
};
pub use mcp::{AllowedMcpServer, McpServerAllowlist, McpServerPolicy, PolicySubjectOrigin};
pub use parse::MANAGED_POLICY_CONFIG_KEYS;
pub use verdict::{McpBlockReason, McpSubject, McpVerdict};

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing::{info, warn};

use layer::{PolicyLayer, PolicyLayerTier};
use parse::{
    McpPolicyList, parse_extra_marketplaces, parse_mcp_entry_list, parse_strict_marketplaces,
    policy_bool,
};

use super::resolution::parse_managed_settings_base;
use crate::permission::rules::DefaultPermissionMode;
use crate::permission::types::{PermissionRule, Sourced};

/// Managed MCP/plugin/marketplace policy from Claude `managed-settings.json` plus every `managed_config.toml` / `requirements.toml` layer.
/// Strictest-wins: any deny wins, every restricted source must allow, pins only tighten. Loaded once per process. No `managed-settings.d/`, MDM, or registry yet.
#[derive(Debug, Default)]
pub struct ManagedSettings {
    pub features: ManagedSettingsFeatures,
    pub permissions: Vec<Sourced<PermissionRule>>,
    /// Parsed `permissions.defaultMode` (highest mode precedence over user files).
    /// Read and populated by the resolution side ([`parse_managed_settings_base`]).
    pub(in crate::permission) default_mode: Option<DefaultPermissionMode>,
    pub mcp_allowlist: McpServerPolicy,
    pub marketplace_allowlist: MarketplacePolicy,
    /// `enableAllProjectMcpServers = false`: drop project MCP unless an
    /// allow entry whose ownership satisfies the pin's grants it.
    pub project_mcp: PolicyPin,
    /// `plugin_auto_update = false`: no session-start plugin auto-update.
    pub plugin_auto_update: PolicyPin,
    /// Marketplaces pinned via managed `extraKnownMarketplaces`.
    pub extra_marketplaces: Vec<ManagedMarketplace>,
}

static MANAGED_SETTINGS: OnceLock<ManagedSettings> = OnceLock::new();

pub fn managed_settings() -> &'static ManagedSettings {
    MANAGED_SETTINGS.get_or_init(load_managed_settings)
}

fn load_managed_settings() -> ManagedSettings {
    let claude = distill_config::claude_managed_settings_path()
        .and_then(|path| read_managed_settings_json(&path).map(|json| (json, path)));
    let toml_layers = managed_toml_policy_layers(
        distill_config::managed_config_layers(),
        distill_config::requirements_layers(),
    );
    resolve_managed_settings(claude, toml_layers)
}

/// Tier-tag the on-disk TOML layers. Split from [`load_managed_settings`] so a
/// test can feed real fixture files through `managed_config_layers_at` and
/// prove layer discovery still reaches the policy engine.
fn managed_toml_policy_layers(
    managed: Vec<distill_config::ManagedConfigLayer>,
    requirements: Vec<distill_config::RequirementsLayer>,
) -> Vec<PolicyLayer> {
    let mut toml_layers: Vec<PolicyLayer> = Vec::new();
    for layer in managed {
        toml_layers.push(PolicyLayer {
            tier: if layer.is_system {
                PolicyLayerTier::SystemManaged
            } else {
                PolicyLayerTier::UserManaged
            },
            path: layer.path,
            value: layer.value,
        });
    }
    for layer in requirements {
        toml_layers.push(PolicyLayer {
            tier: match layer.source {
                distill_config::RequirementsSource::Mdm => PolicyLayerTier::Mdm,
                _ if layer.is_system => PolicyLayerTier::SystemRequirements,
                _ => PolicyLayerTier::UserRequirements,
            },
            path: PathBuf::from(layer.source.label().as_ref()),
            value: layer.value,
        });
    }
    toml_layers
}

/// Pure form of [`load_managed_settings`] over pre-loaded sources.
/// Native TOML layers apply trust-descending; the advisory Claude file applies last so it never claims a name ahead of an admin grok layer.
fn resolve_managed_settings(
    claude: Option<(serde_json::Value, PathBuf)>,
    mut toml_layers: Vec<PolicyLayer>,
) -> ManagedSettings {
    let mut ms = match &claude {
        Some((json, path)) => parse_managed_settings_base(json, path),
        None => ManagedSettings::default(),
    };
    toml_layers.sort_by_key(|layer| layer.tier);
    for layer in toml_layers {
        // Only the policy keys cross TOML → JSON (one parser serves both surfaces),
        // so an unrelated exotic value elsewhere never affects the policy pins.
        let policy_table: toml::map::Map<String, toml::Value> = layer
            .value
            .as_table()
            .map(|t| {
                t.iter()
                    .filter(|(k, _)| MANAGED_POLICY_CONFIG_KEYS.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if policy_table.is_empty() {
            continue;
        }
        match serde_json::to_value(&policy_table) {
            Ok(json) => apply_policy_source(&mut ms, &json, &layer.path, layer.tier),
            Err(e) => {
                // No known TOML value fails here (inf/nan become null, datetimes
                // objects); defensive fail-closed for anything that ever does.
                tracing::error!(
                    path = %layer.path.display(),
                    error = %e,
                    "policy layer could not be read; treating every policy key as malformed (MCP and marketplace lockdown, project MCP and plugin auto-update pinned off)"
                );
                apply_unreadable_policy_source(&mut ms, &layer.path, layer.tier);
            }
        }
    }
    if let Some((json, path)) = &claude {
        // Already JSON, applied unfiltered; Vendor sorts last, so the apply
        // order matches the sorted loop.
        apply_policy_source(&mut ms, json, path, PolicyLayerTier::Vendor);
    }
    ms
}

/// [`parse_managed_settings_base`] plus the file's advisory policy — the Claude-only parse used by tests.
/// The layered runtime path is [`resolve_managed_settings`], which applies advisory policy after every native TOML layer.
#[cfg(test)]
fn parse_managed_settings_json(json: &serde_json::Value, path: &Path) -> ManagedSettings {
    let mut ms = parse_managed_settings_base(json, path);
    apply_policy_source(&mut ms, json, path, PolicyLayerTier::Vendor);
    ms
}

/// Fail-closed stand-in for a layer whose policy keys could not be read: as if
/// every key were present-but-malformed (MCP + marketplace lockdown, pins off).
fn apply_unreadable_policy_source(ms: &mut ManagedSettings, path: &Path, tier: PolicyLayerTier) {
    ms.mcp_allowlist.sources.push(
        McpServerAllowlist::new(Vec::new(), Vec::new(), Some(path.to_path_buf()))
            .with_authority(tier.authority())
            .with_ownership(tier.ownership())
            .with_lockdown(),
    );
    ms.marketplace_allowlist.sources.push(MarketplaceAllowlist {
        allowed_urls: Vec::new(),
        source_path: Some(path.to_path_buf()),
        authority: tier.authority(),
    });
    pin_disabled(&mut ms.project_mcp, path, tier.ownership());
    pin_disabled(&mut ms.plugin_auto_update, path, tier.ownership());
}

/// Tighten-only disable pin: the first pinning layer names the source, but an
/// admin-owned layer upgrades (re-attributes) a user-owned pin, never the reverse.
fn pin_disabled(pin: &mut PolicyPin, path: &Path, ownership: PolicyLayerOwnership) {
    let upgrades = ownership == PolicyLayerOwnership::Admin
        && matches!(
            pin,
            PolicyPin::Disabled {
                ownership: PolicyLayerOwnership::User,
                ..
            }
        );
    if !pin.is_disabled() || upgrades {
        *pin = PolicyPin::Disabled {
            source: path.to_path_buf(),
            ownership,
        };
    }
}

/// Fold one source's pins into `ms` strictest-wins: layers only accumulate, so a later layer can add restrictions but never remove another's.
/// The tier derives authority (how MCP/marketplace restrictions bind; pins are authority-blind) and ownership.
fn apply_policy_source(
    ms: &mut ManagedSettings,
    json: &serde_json::Value,
    path: &Path,
    tier: PolicyLayerTier,
) {
    let authority = tier.authority();
    let ownership = tier.ownership();
    let mcp_allow_entries = parse_mcp_entry_list(json, McpPolicyList::Allow);
    let mcp_deny_entries = parse_mcp_entry_list(json, McpPolicyList::Deny);
    let managed_only = policy_bool(
        json,
        &[
            "allowManagedMcpServersOnly",
            "allow_managed_mcp_servers_only",
        ],
        // Block on error: an unreadable value is treated as enabled.
        true,
        path,
    ) == Some(true);

    // Full lockdown (see the lockdown field docs); an explicit empty deny list stays harmless.
    let lockdown = mcp_allow_entries.locks_down() || mcp_deny_entries.is_malformed();
    if lockdown {
        // The key-level warnings above say what is wrong; this one names the file.
        warn!(
            path = %path.display(),
            "MCP lockdown: the allow list has no usable entries or the deny list is unenforceable; every MCP server this source binds is blocked"
        );
    }
    let allow_entries = mcp_allow_entries.entries();
    let deny_entries = mcp_deny_entries.entries();

    if lockdown || !allow_entries.is_empty() || !deny_entries.is_empty() || managed_only {
        info!(
            path = %path.display(),
            allow = allow_entries.len(),
            deny = deny_entries.len(),
            managed_only,
            ownership = ?ownership,
            lockdown,
            "Loaded MCP server policy"
        );
        let mut allowlist =
            McpServerAllowlist::new(allow_entries, deny_entries, Some(path.to_path_buf()))
                .with_authority(authority)
                .with_ownership(ownership);
        if managed_only {
            allowlist = allowlist.with_managed_only();
        }
        if lockdown {
            allowlist = allowlist.with_lockdown();
        }
        ms.mcp_allowlist.sources.push(allowlist);
    }

    // Both boolean pins fail closed on an invalid value (`false` disables).
    if policy_bool(
        json,
        &[
            "enableAllProjectMcpServers",
            "enable_all_project_mcp_servers",
        ],
        false,
        path,
    ) == Some(false)
    {
        pin_disabled(&mut ms.project_mcp, path, ownership);
    }

    if policy_bool(
        json,
        &["pluginAutoUpdate", "plugin_auto_update"],
        false,
        path,
    ) == Some(false)
    {
        pin_disabled(&mut ms.plugin_auto_update, path, ownership);
    }

    let strict = parse_strict_marketplaces(json);
    if !strict.is_absent() {
        // `Malformed` yields no URLs, and a zero-URL source is a lockdown.
        let allowed_urls = strict.entries();
        if allowed_urls.is_empty() {
            warn!(
                path = %path.display(),
                "marketplace lockdown: the strict list has no usable entries; every marketplace this source binds is blocked"
            );
        }
        info!(
            path = %path.display(),
            count = allowed_urls.len(),
            "Loaded marketplace allowlist"
        );
        ms.marketplace_allowlist.sources.push(MarketplaceAllowlist {
            allowed_urls,
            source_path: Some(path.to_path_buf()),
            authority,
        });
    }

    let extras = parse_extra_marketplaces(json, path, ownership);
    if extras.pin_auto_update_off {
        pin_disabled(&mut ms.plugin_auto_update, path, ownership);
    }
    for extra in extras.entries {
        // First pinning source wins a name, except an admin-owned entry replaces
        // a user-owned claim (a user squat must not drop an admin's Local pin).
        match ms
            .extra_marketplaces
            .iter_mut()
            .find(|m| m.name == extra.name)
        {
            None => ms.extra_marketplaces.push(extra),
            Some(claimed)
                if claimed.ownership == PolicyLayerOwnership::User
                    && extra.ownership == PolicyLayerOwnership::Admin =>
            {
                *claimed = extra;
            }
            Some(_) => {}
        }
    }
}

fn read_managed_settings_json(path: &Path) -> Option<serde_json::Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to read managed-settings.json");
            return None;
        }
    };
    match serde_json::from_str(&content) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to parse managed-settings.json");
            None
        }
    }
}

#[derive(Debug, Default)]
pub struct ManagedSettingsFeatures {
    pub disable_telemetry: Option<bool>,
    pub disable_feedback: Option<bool>,
    pub disable_yolo: Option<bool>,
    pub source_path: Option<std::path::PathBuf>,
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
