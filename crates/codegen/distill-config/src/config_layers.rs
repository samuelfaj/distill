// Modified for Distill by Samuel Fajreldines, 2026.
//! The layer *files* are read by [`crate::loader`].
//! This module owns how those layers combine into the effective config: layer precedence, the `GROK_CONFIG` overlay, and campaign resolution.

use crate::loader::{
    deep_merge_toml, load_from_disk, load_managed_config, load_system_managed_config,
    normalize_config_layer,
};
use crate::validation::{load_requirements, load_system_requirements};

/// Whether a layer merge includes the `GROK_CONFIG` overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayInclusion {
    Include,
    Exclude,
}

/// Layers from lowest to highest priority. `[[campaigns]]` is taken off each layer at load.
#[derive(Clone)]
pub struct ConfigLayers {
    pub system_managed: toml::Value,
    pub managed: toml::Value,
    pub user: toml::Value,
    /// `GROK_CONFIG` / `GROK_CONFIG_PATH` overlay, above user but below requirements.
    /// Soft settings only; this doc is the canonical source of truth for what the overlay can and cannot reach.
    /// This is fail-closed: every code-exec, auth, egress, trust, or discovery table is absent from the allowlist and dropped by default.
    pub env_overlay: Option<toml::Value>,
    pub user_requirements: Option<toml::Value>,
    pub system_requirements: Option<toml::Value>,
    /// macOS MDM requirements; highest requirements tier when present.
    pub mdm_requirements: Option<toml::Value>,
    pub campaigns: crate::campaigns::CampaignOverrides,
}

impl Default for ConfigLayers {
    fn default() -> Self {
        Self {
            system_managed: toml::Value::Table(Default::default()),
            managed: toml::Value::Table(Default::default()),
            user: toml::Value::Table(Default::default()),
            env_overlay: None,
            user_requirements: None,
            system_requirements: None,
            mdm_requirements: None,
            campaigns: crate::campaigns::CampaignOverrides::default(),
        }
    }
}

impl ConfigLayers {
    pub fn load() -> std::io::Result<Self> {
        use crate::campaigns::{CampaignOverrides, take_campaign_entries};

        let mut system_managed = load_system_managed_config()?;
        let system_managed_campaigns = take_campaign_entries(&mut system_managed, "system_managed");

        let mut managed = load_managed_config()?;
        let managed_campaigns = take_campaign_entries(&mut managed, "managed");

        let mut user = load_from_disk()?;
        let user_campaigns = take_campaign_entries(&mut user, "user");

        let env_overlay = crate::env_overlay::load_env_overlay();

        let mut user_requirements = load_requirements();
        let mut system_requirements = load_system_requirements();
        let mut mdm_requirements = crate::validation::mdm_requirements_value();

        // Highest-authority tier first: `merge_campaign_entries` is first-id-wins, so a duplicate campaign id must resolve mdm > system > user
        // That matches the layer precedence in `effective_config_base`, where mdm is merged last/highest
        let mut requirements_campaigns = Vec::new();
        if let Some(ref mut req) = mdm_requirements {
            requirements_campaigns.extend(take_campaign_entries(req, "requirements"));
        }
        if let Some(ref mut req) = system_requirements {
            requirements_campaigns.extend(take_campaign_entries(req, "requirements"));
        }
        if let Some(ref mut req) = user_requirements {
            requirements_campaigns.extend(take_campaign_entries(req, "requirements"));
        }

        // Normalize each layer before any merge, so `[toolset.web_search]`'s `allowed_domains` / `excluded_domains` travel together
        // A layer that sets one clears the other to `[]`
        // That makes `deep_merge_toml` replace the whole policy from the winning layer instead of mixing keys across layers
        normalize_config_layer(&mut system_managed);
        normalize_config_layer(&mut managed);
        normalize_config_layer(&mut user);
        for req in [
            &mut user_requirements,
            &mut system_requirements,
            &mut mdm_requirements,
        ]
        .into_iter()
        .flatten()
        {
            normalize_config_layer(req);
        }

        Ok(Self {
            system_managed,
            managed,
            user,
            env_overlay,
            user_requirements,
            system_requirements,
            mdm_requirements,
            campaigns: CampaignOverrides {
                requirements: requirements_campaigns,
                user: user_campaigns,
                managed: managed_campaigns,
                system_managed: system_managed_campaigns,
            },
        })
    }

    /// Layer merge (no campaigns), including the `GROK_CONFIG` overlay.
    /// Overlay-inclusive: security gates must not read this.
    /// Use [`Self::effective_config_base_without_overlay`] for any gate (the overlay-free set is enumerated on [`Self::env_overlay`]).
    pub fn effective_config_base(&self) -> toml::Value {
        self.merge(OverlayInclusion::Include)
    }

    /// Layer merge excluding the `GROK_CONFIG` overlay, for security gates.
    pub fn effective_config_base_without_overlay(&self) -> toml::Value {
        self.merge(OverlayInclusion::Exclude)
    }

    fn merge(&self, inclusion: OverlayInclusion) -> toml::Value {
        let Self {
            system_managed,
            managed,
            user,
            env_overlay,
            user_requirements: _,
            system_requirements: _,
            mdm_requirements: _,
            campaigns: _,
        } = self;
        let mut merged = system_managed.clone();
        deep_merge_toml(&mut merged, managed);
        deep_merge_toml(&mut merged, user);
        if let (OverlayInclusion::Include, Some(overlay)) = (inclusion, env_overlay) {
            deep_merge_toml(&mut merged, overlay);
        }
        for req in self.requirements_in_order() {
            deep_merge_toml(&mut merged, req);
        }
        merged
    }

    fn requirements_in_order(&self) -> impl Iterator<Item = &toml::Value> {
        [
            &self.user_requirements,
            &self.system_requirements,
            &self.mdm_requirements,
        ]
        .into_iter()
        .flatten()
    }

    /// Campaign source slices in priority order (first id wins): requirements > remote > user > managed > system_managed.
    /// This is the single source of truth for the precedence; both this crate and the shell resolver consume it.
    pub fn campaign_source_slices<'a>(
        &'a self,
        remote_campaigns: &'a [crate::campaigns::CampaignEntry],
    ) -> [&'a [crate::campaigns::CampaignEntry]; 5] {
        [
            &self.campaigns.requirements,
            remote_campaigns,
            &self.campaigns.user,
            &self.campaigns.managed,
            &self.campaigns.system_managed,
        ]
    }

    /// Active campaigns against `base`: the kill switch, then the priority merge (first-id-wins), then dropping dismissed ids.
    /// This is the one place that resolves disk campaigns; the shell wraps it with the `GROK_CAMPAIGNS_OVERRIDE` env layer.
    pub fn resolve_campaigns(
        &self,
        base: &toml::Value,
        remote_campaigns: &[crate::campaigns::CampaignEntry],
        dismissed_ids: &std::collections::HashSet<String>,
    ) -> Vec<crate::campaigns::CampaignEntry> {
        if campaigns_application_disabled(base) {
            return Vec::new();
        }
        let merged = crate::campaigns::merge_campaign_entries(
            &self.campaign_source_slices(remote_campaigns),
        );
        crate::campaigns::filter_active_campaigns(merged, dismissed_ids)
    }

    /// Re-merge the requirements layers so an admin's `requirements.toml` always wins over a campaign overlay, whatever the campaign's source layer.
    /// Campaigns are full-power (any field), so this is the structural guarantee that a lower-trust campaign can't override an admin-set field.
    fn reapply_requirements(&self, merged: &mut toml::Value) {
        for req in self.requirements_in_order() {
            deep_merge_toml(merged, req);
        }
    }

    /// Apply campaign patches, re-apply the `GROK_CONFIG` overlay, then restore requirements.
    pub fn apply_campaign_overrides(
        &self,
        merged: &mut toml::Value,
        active: &[crate::campaigns::CampaignEntry],
    ) {
        crate::campaigns::apply_active_campaign_patches(merged, active);
        if let Some(overlay) = &self.env_overlay {
            deep_merge_toml(merged, overlay);
        }
        self.reapply_requirements(merged);
    }

    /// Layer merge and disk/remote campaign overlay, honoring the kill switch.
    /// The shell's `load_effective_config` is the remote/override-aware path; this is used by `effective_config_disk_only` and tests.
    pub fn effective_config_with_campaigns(
        &self,
        remote_campaigns: &[crate::campaigns::CampaignEntry],
        dismissed_ids: &std::collections::HashSet<String>,
    ) -> toml::Value {
        let mut merged = self.effective_config_base();
        let active = self.resolve_campaigns(&merged, remote_campaigns, dismissed_ids);
        self.apply_campaign_overrides(&mut merged, &active);
        merged
    }

    /// Disk campaigns and on-disk dismiss (`campaigns_state.json`); **no remote, no env override**.
    /// The name makes the divergence from the shell's remote-aware `load_effective_config` explicit at every call site.
    pub fn effective_config_disk_only(&self) -> toml::Value {
        self.effective_config_with_campaigns(&[], &load_dismissed_ids_from_home())
    }

    pub fn has_managed(&self) -> bool {
        self.managed.as_table().is_some_and(|t| !t.is_empty())
            || self
                .system_managed
                .as_table()
                .is_some_and(|t| !t.is_empty())
    }

    pub fn has_system_managed(&self) -> bool {
        self.system_managed
            .as_table()
            .is_some_and(|t| !t.is_empty())
    }
}

/// `GROK_CAMPAIGNS=0` or `[features] campaigns = false` on pre-campaign base.
pub fn campaigns_application_disabled(base_effective: &toml::Value) -> bool {
    if crate::env_bool("GROK_CAMPAIGNS") == Some(false) {
        return true;
    }
    base_effective
        .get("features")
        .and_then(|f| f.get("campaigns"))
        .and_then(|c| c.as_bool())
        == Some(false)
}

/// Process-global `GROK_CAMPAIGNS` lock. A mutex local to the setter is not
/// enough because `effective_config_with_campaigns` also reads the var.
#[cfg(test)]
pub(crate) fn lock_grok_campaigns_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Disk layers only (no remote, no env override).
/// Prefer `distill_shell::util::config::load_effective_config` when remote campaigns or `GROK_CAMPAIGNS_OVERRIDE` must be honored.
/// The name mirrors [`ConfigLayers::effective_config_disk_only`] so the divergence from the remote-aware loader is explicit at every call site.
pub fn load_effective_config_disk_only() -> std::io::Result<toml::Value> {
    Ok(ConfigLayers::load()?.effective_config_disk_only())
}

/// On-disk campaign dismiss state.
/// This is the single source of truth for the file's name, location, and JSON shape.
/// The shell's writer reuses these so the read and write sides can't drift.
pub const CAMPAIGNS_STATE_FILE: &str = "campaigns_state.json";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CampaignsState {
    #[serde(default)]
    pub dismissed_ids: Vec<String>,
}

/// Path to `$GROK_HOME/campaigns_state.json` under `home`.
pub fn campaigns_state_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(CAMPAIGNS_STATE_FILE)
}

/// Fail-open dismissed ids from `$GROK_HOME/campaigns_state.json`.
pub fn load_dismissed_ids_from_home() -> std::collections::HashSet<String> {
    let Some(home) = crate::user_distill_home() else {
        return std::collections::HashSet::new();
    };
    let Ok(contents) = std::fs::read_to_string(campaigns_state_path(&home)) else {
        return std::collections::HashSet::new();
    };
    serde_json::from_str::<CampaignsState>(&contents)
        .map(|s| s.dismissed_ids.into_iter().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_config_mdm_requirements_win_over_system_and_user() {
        // MDM is merged last, so an admin-forced value clamps the effective config over both the user config and the system requirements layer
        let layers = ConfigLayers {
            user: toml::from_str("[features]\nweb_fetch = true\n").unwrap(),
            system_requirements: Some(toml::from_str("[features]\nweb_fetch = true\n").unwrap()),
            mdm_requirements: Some(toml::from_str("[features]\nweb_fetch = false\n").unwrap()),
            ..Default::default()
        };
        assert_eq!(
            layers
                .effective_config_disk_only()
                .get("features")
                .and_then(|f| f.get("web_fetch"))
                .and_then(toml::Value::as_bool),
            Some(false),
        );
    }

    /// `GROK_CAMPAIGNS=0` disables campaign application regardless of config.
    #[test]
    fn kill_switch_env_var_disables() {
        let _g = lock_grok_campaigns_env();
        let prior = std::env::var_os("GROK_CAMPAIGNS");
        let empty = toml::Value::Table(Default::default());

        // SAFETY: `lock_grok_campaigns_env` serializes this against every test that
        // mutates or reads GROK_CAMPAIGNS.
        unsafe { std::env::set_var("GROK_CAMPAIGNS", "0") };
        assert!(campaigns_application_disabled(&empty));

        unsafe { std::env::remove_var("GROK_CAMPAIGNS") };
        assert!(!campaigns_application_disabled(&empty));

        match prior {
            Some(v) => unsafe { std::env::set_var("GROK_CAMPAIGNS", v) },
            None => unsafe { std::env::remove_var("GROK_CAMPAIGNS") },
        }
    }

    #[test]
    fn env_overlay_precedence_and_overlay_free_merge() {
        let _env = lock_grok_campaigns_env();
        let mut layers = ConfigLayers {
            user: toml::from_str("[models]\ndefault = \"user\"\n[telemetry]\nmode = \"on\"\n")
                .unwrap(),
            env_overlay: Some(
                toml::from_str(
                    "[models]\ndefault = \"overlay\"\ndefault_reasoning_effort = \"high\"\n",
                )
                .unwrap(),
            ),
            ..Default::default()
        };
        layers.campaigns.managed = vec![crate::campaigns::CampaignEntry {
            id: "c1".into(),
            patch: toml::from_str("[models]\ndefault = \"campaign\"\n").unwrap(),
        }];
        let none = std::collections::HashSet::new();

        let with_overlay: toml::Value = toml::from_str(
            "[models]\ndefault = \"overlay\"\ndefault_reasoning_effort = \"high\"\n\
             [telemetry]\nmode = \"on\"\n",
        )
        .unwrap();
        assert_eq!(
            layers.effective_config_with_campaigns(&[], &none),
            with_overlay
        );

        let overlay_free: toml::Value =
            toml::from_str("[models]\ndefault = \"user\"\n[telemetry]\nmode = \"on\"\n").unwrap();
        assert_eq!(layers.effective_config_base_without_overlay(), overlay_free);

        layers.user_requirements =
            Some(toml::from_str("[models]\ndefault = \"pinned\"\n").unwrap());
        let clamped: toml::Value = toml::from_str(
            "[models]\ndefault = \"pinned\"\ndefault_reasoning_effort = \"high\"\n\
             [telemetry]\nmode = \"on\"\n",
        )
        .unwrap();
        assert_eq!(layers.effective_config_with_campaigns(&[], &none), clamped);
    }
}
