// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use serde::Deserialize;

/// The announcements this harness shows, filtered: none unless the owner asked.
///
/// Every producer goes through here — the settings push and the event loop's
/// resolved-settings path both fill `active_announcements`, and a gate on only
/// one of them is a banner that shows up anyway.
pub(crate) fn visible_announcements(
    announcements: Vec<distill_announcements::RemoteAnnouncement>,
) -> Vec<distill_announcements::RemoteAnnouncement> {
    if announcements_enabled() {
        distill_announcements::filter_expired(announcements)
    } else {
        Vec::new()
    }
}

/// Whether the backend's announcement banners are shown.
///
/// Off unless the owner asks for them: the banner is provider marketing, and a
/// harness that routes to several providers should not advertise one of them.
fn announcements_enabled() -> bool {
    if let Ok(value) = std::env::var("REMOTE_CODE_ANNOUNCEMENTS") {
        return !value.trim().is_empty() && value.trim() != "0";
    }
    distill_shell::config::ConfigLayers::load()
        .map(|layers| layers.effective_config_base_without_overlay())
        .ok()
        .and_then(|merged| {
            merged
                .get("announcements")
                .and_then(|section| section.get("enabled"))
                .and_then(toml::Value::as_bool)
        })
        .unwrap_or(false)
}

/// Handle `x.ai/models/update`: the model list changed (etag-triggered refresh).
pub(super) fn handle_models_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    if let Ok(model_state) = serde_json::from_str::<acp::SessionModelState>(notif.params.get()) {
        use crate::acp::model_state::ModelState;
        let new_models = ModelState::from(Some(model_state));
        tracing::info!(
            count = new_models.available.len(),
            "models updated via x.ai/models/update"
        );

        app.models.update_catalog(new_models.available.clone());
        let stale = app
            .models
            .current
            .as_ref()
            .is_none_or(|id| !app.models.available.contains_key(id));
        if stale && let Some(id) = new_models.current {
            app.models.set_current(id, None);
        }

        for agent in app.agents.values_mut() {
            if let Some(ref current) = agent.session.models.current
                && !new_models.available.contains_key(current)
            {
                tracing::debug!(
                    current_model = %current.0,
                    available_count = new_models.available.len(),
                    "models update dropped this session's model from the catalog; keeping it displayed"
                );
            }
            agent
                .session
                .models
                .update_catalog(new_models.available.clone());
        }
        true
    } else {
        tracing::warn!("Failed to parse x.ai/models/update");
        false
    }
}

/// Handle `x.ai/settings/update`: remote settings refreshed on `/new`.
pub(super) fn handle_settings_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(update) = serde_json::from_str::<PagerSettingsUpdate>(notif.params.get()) else {
        tracing::warn!("Failed to parse x.ai/settings/update");
        return false;
    };

    // Reseed this process's remote-campaign cache
    // Without this reseed a remote campaign stays invisible to `resolve_dismissable_campaigns`
    // A `/model` pick then never records its dismissal and the leader re-nudges every new session
    if let Some(campaigns) = update.campaigns.clone() {
        let rs = distill_shell::util::config::RemoteSettings {
            campaigns,
            ..Default::default()
        };
        distill_shell::util::config::set_remote_campaigns_from_settings(Some(&rs));
    }

    if let Some(v) = update.auto_permission_mode_enabled {
        // Keep the pager's auto-permission-mode gate live with the remote settings tier
        // The leader caches it agent-side; the pager process needs its own copy
        // Refresh the startup snapshot so the Shift+Tab cycle and the settings modal both reflect a remote-only enablement or kill-switch without a restart
        distill_shell::util::config::cache_remote_auto_permission_mode_enabled(Some(v));
        app.auto_mode_gate = distill_shell::util::config::auto_permission_mode_enabled_from_disk();
        // Mid-session kill switch: when the gate just went off, drop displayed Auto to Ask and clear every agent's per-session flag
        // Clearing only the display would let the agent keep classifier-approving while the UI shows "Ask"
        // The emergency-off must actually disable enforcement
        if !app.auto_mode_gate {
            // Sessions to notify: agents that HAD Auto on (capture before the downgrade clears the flag) and have a live session id
            let leaving_auto: Vec<acp::SessionId> = app
                .agents
                .values()
                .filter(|a| a.session.is_auto())
                .filter_map(|a| a.session.session_id.clone())
                .collect();
            super::super::dispatch::downgrade_displayed_auto_if_gated(app);
            notify_sessions_leave_auto(app, &leaving_auto);
        }
        // Reveal or hide `/auto` everywhere slash commands appear, in lockstep with the gate (covers both a mid-session kill-switch and re-enablement)
        app.sync_permission_mode_slash_gate();
    }

    if let Some(v) = update.prompt_suggestions_enabled {
        distill_shell::util::config::cache_remote_prompt_suggestions_enabled(Some(v));
    }

    // `permission_mode` is presence-aware (omit / null / string)
    // While the soft default still owns the mode, a push refreshes `default_yolo` and the UI for the next `/new`
    // Once the user claims a mode (Shift+Tab, settings, `/mode`) the latch is cleared and pushes leave it alone
    if let Some(remote_opt) = update.permission_mode.as_ref()
        && app.permission_mode_from_soft_default
    {
        // One config read at the I/O boundary; the applier is deterministic.
        let root = distill_shell::config::load_effective_config()
            .unwrap_or_else(|_| broken_config_ask_fallback());
        apply_soft_default_permission_mode(app, root.get("ui"), remote_opt.as_deref());
    }

    if let Some(v) = update.show_resolved_model {
        app.show_resolved_model = v;
    }
    // Temporary client kill switch: ignore remote `sharing_enabled` until session share links are restored
    // Presence is still observed so a later re-enable can go back to `app.sharing_enabled = v`
    if update.sharing_enabled.is_some() {
        app.sharing_enabled = false;
        for agent in app.agents.values_mut() {
            agent.set_sharing_enabled(false);
        }
    }
    // Env overrides win over live updates too, mirroring the startup resolution in event_loop
    // Otherwise the proxy's explicit `false` (sent as a kill switch) clobbers a local test override moments after launch
    if let Some(v) = update.privacy_notice_rollout {
        app.privacy_notice_rollout =
            distill_config::env_bool("GROK_PRIVACY_NOTICE_ROLLOUT").unwrap_or(v);
    }
    if let Some(v) = update.privacy_banner_reshow_days {
        app.privacy_banner_reshow_days = Some(
            std::env::var("GROK_PRIVACY_BANNER_RESHOW_DAYS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(v),
        );
    }
    // Tier before voice: the same payload may set "API Key" and voice_mode_enabled=false
    // Always recompute is_api_key_auth from the tier so a later Free/SuperGrok stamp does not leave the API-key bypass or a hidden billing surface stuck
    if let Some(v) = update.subscription_tier_display {
        let was_api_key = app.is_api_key_auth;
        let is_key = super::super::app_view::is_api_key_label(&v);
        app.is_api_key_auth = is_key;
        app.usage_visible = app.team_name.is_none() && app.consumer_account();
        app.sync_billing_surface_to_agents();
        app.subscription_tier = Some(v);
        app.apply_tier_restrictions();
        // Leaving API Key for free/X Basic without a voice field drops the forced-on voice
        // Paid tiers keep voice; remote settings may send voice_mode_enabled later.
        if was_api_key
            && !is_key
            && update.voice_mode_enabled.is_none()
            && app
                .subscription_tier
                .as_deref()
                .is_some_and(distill_shell::tier::is_restricted_tier_name)
        {
            app.voice_reset();
            app.voice_ui_active = false;
            app.apply_voice_mode_enabled(false);
        }
    }
    if let Some(remote_v) = update.dock_enabled {
        crate::views::dock::set_enabled(crate::app::resolve_dock_enabled(Some(remote_v)));
    }
    if let Some(remote_v) = update.terminal_theme_enabled {
        let enabled = crate::app::resolve_terminal_theme_enabled(Some(remote_v));
        crate::theme::cache::set_terminal_theme_enabled(enabled);
        // Kill switch: a selected terminal theme falls back to what config resolves to without it (its name no longer parses, and the flip dropped the cached auto overrides), matching the next startup.
        // `selected_kind` sees through the minimal lock's `current_kind()` masking so a locked session can't revive the theme when the lock lifts; under the lock only the stored kind needs correcting (`apply_kind` no-ops there), the visual stays pinned.
        // The reveal direction is deliberately restart-only for an explicit `theme = "terminal"` (no surprise swap mid-session); auto mode picks it up on the next appearance change via the invalidated auto cache.
        if !enabled && crate::theme::cache::selected_kind().is_terminal_native() {
            let fallback = crate::theme::cache::resolve_initial_theme_no_osc11();
            if crate::theme::cache::terminal_native_locked() {
                crate::theme::cache::set(fallback);
            } else {
                crate::theme::Theme::apply_kind(fallback);
            }
        }
    }
    if let Some(remote_v) = update.voice_mode_enabled {
        let v = crate::app::resolve_voice_mode_live(Some(remote_v), app.is_api_key_auth);
        if !v {
            app.voice_reset();
            app.voice_ui_active = false;
        }
        app.apply_voice_mode_enabled(v);
    } else {
        app.ensure_voice_for_api_key();
    }
    // TODO: extract resolve_session_picker_grouped helper (duplicates event_loop.rs:143-160)
    // The env var beats config, which beats remote (mirrors event_loop.rs startup)
    if let Some(remote_val) = update.session_picker_grouped {
        let resolved = std::env::var("GROK_SESSION_PICKER_GROUPED")
            .ok()
            .and_then(|v| match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            })
            .or_else(|| {
                distill_shell::config::load_effective_config()
                    .ok()
                    .and_then(|cfg| cfg.get("cli")?.get("session_picker_grouped")?.as_bool())
            })
            .unwrap_or(remote_val);
        app.session_picker_grouped = resolved;
    }
    if let Some(v) = update.subscription_watch_interval_secs {
        app.subscription_watch_interval_secs = Some(v);
    }

    // Neither condition met: don't touch the gate
    // In particular, allow_access=Some(false) without a gate_message must NOT clear the gate
    // (gate_from_settings returns None when gate_message is absent, which would incorrectly lift an existing gate.)

    // A fresh machine has no auth at startup, so the prefetch never runs and the startup seed sees no settings
    // Welcome only: seeding the gate behind a session would block new sessions on an unseen screen
    if let Some(gate) = update.consent_gate.as_ref()
        && matches!(app.consent_state, crate::app::consent::ConsentState::Done)
        && matches!(app.active_view, crate::app::app_view::ActiveView::Welcome)
        && app.only_unused_home_or_empty()
    {
        crate::app::event_loop::seed_consent_state_from_gate(app, Some(gate));
        if matches!(
            app.consent_state,
            crate::app::consent::ConsentState::Pending { .. }
        ) {
            // The husk was created while consent still looked Done. Drop it
            // so accept recreates after the gate, and decline does not keep it.
            let abandoned = crate::app::dispatch::abandon_unused_home_session(app);
            app.pending_effects.extend(abandoned);
        }
    }

    if update.allow_access == Some(true) {
        let effs = app.lift_gate();
        app.pending_effects.extend(effs);
    } else if let Some(msg) = update.gate_message.as_ref()
        && !msg.is_empty()
    {
        // (An empty gate_message would only clear the gate message text, NOT access, so it does not touch the gate here.)
        let effs = app.impose_gate(distill_login::GateInfo {
            message: msg.clone(),
            url: update.gate_url.clone(),
            label: update.gate_label.clone(),
        });
        app.pending_effects.extend(effs);
    }

    // Load config layers once for the tips, group_tool_verbs, and collapsed_edit_blocks resolution
    // Loaded unconditionally: the UI flags re-resolve on every update (see below)
    // Updates are rare (post-auth refresh, `/new`), so three small TOML reads are fine
    let (requirements, user_config, managed_config) = (
        distill_shell::config::load_merged_requirements(),
        distill_shell::config::load_from_disk().ok(),
        distill_shell::config::load_managed_config().ok(),
    );

    // Runs on None too: the shell always publishes this field from its live remote tier
    // So None means remote settings cleared it, or an older shell cannot deliver the remote tier at all
    // Either way resolving without a remote value is correct
    let remote = distill_shell::util::config::RemoteSettings {
        group_tool_verbs: update.group_tool_verbs,
        ..Default::default()
    };
    let resolved = distill_shell::util::config::resolve_group_tool_verbs(
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
        Some(&remote),
    )
    .value;
    // On a real flip, re-fold every live transcript (mirrors dispatch's set_group_tool_verbs_inner); unchanged values keep `/new` cheap
    // Stale expansion ids describe the old grouping shape
    // Drop them so the re-fold can't reopen a verb slot expanded or mark a coincident dense group expanded (see `clear_group_expansion`)
    if resolved != crate::appearance::cache::load_group_tool_verbs() {
        crate::appearance::cache::set_group_tool_verbs(resolved);
        for agent in app.agents.values_mut() {
            agent.scrollback.clear_group_expansion();
            agent.scrollback.invalidate_heights();
            for child in agent.subagent_views.values_mut() {
                child.scrollback.clear_group_expansion();
                child.scrollback.invalidate_heights();
            }
        }
    }

    // Same rule as group_tool_verbs above: None also reverts
    // Re-resolve the full local chain with the pushed remote tier so a cleared remote field falls back to local/default instead of staying latched
    let remote = distill_shell::util::config::RemoteSettings {
        collapsed_edit_blocks: update.collapsed_edit_blocks,
        ..Default::default()
    };
    let resolved = distill_shell::util::config::resolve_collapsed_edit_blocks(
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
        Some(&remote),
    )
    .value;
    // On a real flip, rebuild on-default Edit rows and repaint suffixes in every live transcript (mirrors dispatch's set_collapsed_edit_blocks_inner)
    // Unchanged values keep `/new` cheap
    let prev = crate::appearance::cache::load_collapsed_edit_blocks();
    if resolved != prev {
        crate::appearance::cache::set_collapsed_edit_blocks(resolved);
        for agent in app.agents.values_mut() {
            agent
                .scrollback
                .apply_collapsed_edit_blocks_flip(prev, resolved);
            for child in agent.subagent_views.values_mut() {
                child
                    .scrollback
                    .apply_collapsed_edit_blocks_flip(prev, resolved);
            }
        }
    }

    // A live session's scheduled fires keep the mode the shell pinned when the session's actor spawned
    // Applying a pushed flip here would make `/loop` promise a runtime those fires never get

    // Re-resolve tips from config layers and the updated remote tips
    if let Some(remote_tips) = update.tips {
        use distill_shell::util::config::resolve_tips;

        app.tips = resolve_tips(
            requirements.as_ref(),
            user_config.as_ref(),
            managed_config.as_ref(),
            Some(&remote_tips),
        );
        if !app.tips.is_empty() {
            let distill_home = distill_tools::util::distill_home::distill_home();
            app.tip = distill_shell::util::tips::pick_and_advance(&app.tips, &distill_home);
        } else {
            app.tip = None;
        }
    }

    // Re-resolve dropdown tags only when the update carries the field
    // Outer None means the field is absent (older shell), so keep the tags resolved at startup
    // Env and local [slash_command_tags] always apply via resolve_slash_command_tags
    if let Some(remote_tags) = update.slash_command_tags.as_ref() {
        use distill_shell::util::config::resolve_slash_command_tags;
        let effective_config = distill_shell::config::load_effective_config().ok();
        let empty_toml = toml::Value::Table(Default::default());
        let tags_config = effective_config.as_ref().unwrap_or(&empty_toml);
        *app.command_tags.borrow_mut() =
            resolve_slash_command_tags(tags_config, remote_tags.as_ref());
    }

    tracing::info!("settings updated via x.ai/settings/update");
    true
}

/// Failed config load: pin explicit ask so a remote push cannot escalate.
pub(super) fn broken_config_ask_fallback() -> toml::Value {
    let mut ui = toml::value::Table::new();
    ui.insert("permission_mode".into(), toml::Value::String("ask".into()));
    let mut root = toml::value::Table::new();
    root.insert("ui".into(), toml::Value::Table(ui));
    toml::Value::Table(root)
}

/// Set up the next `/new` from a pushed `permission_mode`.
/// Live sessions stay untouched; nothing is persisted.
pub(super) fn apply_soft_default_permission_mode(
    app: &mut AppView,
    effective_ui: Option<&toml::Value>,
    remote: Option<&str>,
) {
    let mode = distill_shell::util::config::selected_permission_mode(effective_ui, remote)
        .unwrap_or_else(distill_shell::util::config::default_interactive_permission_mode);
    app.default_yolo = mode.is_always_approve() && app.yolo_policy_block.is_none();
    let auto = mode.is_auto() && app.auto_mode_gate && !app.default_yolo;
    app.current_ui.permission_mode = Some(if auto {
        "auto".to_string()
    } else if app.default_yolo {
        "always-approve".to_string()
    } else {
        distill_shell::util::config::resolved_display_permission_mode(effective_ui, remote)
            .to_string()
    });
}

/// Tell live sessions to leave Auto on the mid-session kill-switch.
/// `yolo_mode` is deliberately OMITTED: the agent skips the yolo branch when the key is absent.
/// A sibling tab's always-approve is thus preserved; only auto is cleared.
pub(super) fn notify_sessions_leave_auto(app: &AppView, session_ids: &[acp::SessionId]) {
    if session_ids.is_empty() {
        return;
    }
    let params = serde_json::json!({
        "auto_mode": false,
        "permission_mode": "ask",
    });
    let notification = acp::ExtNotification::new(
        "x.ai/yolo_mode_changed",
        serde_json::value::to_raw_value(&params)
            .expect("serialize yolo_mode_changed params")
            .into(),
    );
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let args = distill_acp_lib::AcpArgs {
        request: notification,
        response_tx,
    };
    let _ = app.acp_tx.send(args.into());
}

/// Handle `x.ai/sessions/changed`: the leader broadcasts roster upserts/removals to all clients (FleetView dashboard).
pub(super) fn handle_sessions_changed(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(changed) = serde_json::from_str::<crate::app::roster::RosterChanged>(notif.params.get())
    else {
        tracing::warn!("Failed to parse x.ai/sessions/changed");
        return false;
    };
    let mut affected = false;
    for entry in changed.upserted {
        app.upsert_roster_entry(entry);
        affected = true;
    }
    for sid in changed.removed {
        app.remove_roster_entry(&sid);
        affected = true;
    }
    affected
}

pub(super) fn handle_announcements_update(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(parsed) =
        serde_json::from_str::<distill_announcements::AnnouncementsRefreshed>(notif.params.get())
    else {
        return false;
    };

    if parsed.r#gen <= app.announcements_last_gen {
        return false;
    }

    // Re-merge config layers like startup does: the push carries the remote list only
    // A wholesale replace would drop requirements/user/managed announcements and let the prune erase their persisted hide keys
    // The settings handler performs the same disk reads; pushes are rare
    let requirements = distill_shell::config::load_merged_requirements();
    let user_config = distill_shell::config::load_from_disk().ok();
    let managed_config = distill_shell::config::load_managed_config().ok();
    apply_announcements_update(
        app,
        parsed.r#gen,
        &parsed.announcements,
        requirements.as_ref(),
        user_config.as_ref(),
        managed_config.as_ref(),
    );
    true
}

/// Apply half of [`handle_announcements_update`], with config layers injected so the merge/prune behavior is unit-testable without disk state.
/// `resolve_announcements` honors `GROK_ANNOUNCEMENTS_OVERRIDE` first, so a backend push can't reintroduce announcements when the override is set.
pub(super) fn apply_announcements_update(
    app: &mut AppView,
    next_gen: u64,
    remote: &[distill_announcements::RemoteAnnouncement],
    requirements: Option<&toml::Value>,
    user_config: Option<&toml::Value>,
    managed_config: Option<&toml::Value>,
) {
    let merged = distill_shell::util::config::resolve_announcements(
        requirements,
        user_config,
        managed_config,
        Some(remote),
    );
    // Distill ships with the provider's promotional banners off: they are
    // marketing served by the backend, not part of the harness.
    // `[announcements] enabled = true` in the user config, or
    // `REMOTE_CODE_ANNOUNCEMENTS=1`, brings them back.
    let announcements = visible_announcements(merged);

    app.announcement = match app.announcement.as_ref() {
        Some(current) => announcements
            .iter()
            .find(|a| *a == current)
            .cloned()
            .or_else(|| pick_random_announcement(&announcements)),
        None => pick_random_announcement(&announcements),
    };
    app.active_announcements = announcements;
    app.announcements_last_gen = next_gen;
    // Opportunistic per-ID prune on a real update (never per frame) so the hidden set cannot grow unboundedly.
    if distill_announcements::prune_hidden_announcement_ids(
        &mut app.hidden_announcement_ids,
        &app.active_announcements,
    ) {
        app.pending_effects
            .push(Effect::PersistAnnouncementsHidden {
                hidden_ids: app.hidden_announcement_ids.clone(),
            });
    }
    app.sync_session_announcement_slash_gate();
}

pub(super) fn pick_random_announcement(
    announcements: &[distill_announcements::RemoteAnnouncement],
) -> Option<distill_announcements::RemoteAnnouncement> {
    if announcements.is_empty() {
        return None;
    }
    use rand::Rng;
    let idx = rand::rng().random_range(0..announcements.len());
    announcements.get(idx).cloned()
}

/// Deserialization type for the `x.ai/settings/update` notification payload.
/// This side derives `Deserialize` and consumes only the fields the TUI uses.
/// Separate structs keep the pager decoupled from shell internals (a shell-only field needs no pager change).
#[derive(serde::Deserialize)]
pub(super) struct PagerSettingsUpdate {
    #[serde(default)]
    show_resolved_model: Option<bool>,
    #[serde(default)]
    sharing_enabled: Option<bool>,
    #[serde(default)]
    privacy_notice_rollout: Option<bool>,
    #[serde(default)]
    privacy_banner_reshow_days: Option<u64>,
    #[serde(default)]
    voice_mode_enabled: Option<bool>,
    #[serde(default)]
    dock_enabled: Option<bool>,
    #[serde(default)]
    terminal_theme_enabled: Option<bool>,
    #[serde(default)]
    session_picker_grouped: Option<bool>,
    #[serde(default)]
    tips: Option<Vec<String>>,
    /// Free-form per-command slash-dropdown tags, keyed by canonical command name.
    /// Presence-aware and tolerant: omit means no update (older shell), `null` means remote cleared, and a map sets the tags.
    /// Malformed input warns and is treated as absent so a bad value never fails the whole `PagerSettingsUpdate` parse.
    #[serde(default, deserialize_with = "deserialize_settings_update_tags")]
    slash_command_tags: Option<Option<std::collections::BTreeMap<String, String>>>,
    // `announcements` is deliberately NOT consumed here
    // Every shell writer of remote_settings also emits gen-ordered `x.ai/announcements/update` (emit_announcements_if_changed)
    // `None`/omitted (settings-less push, older shell) must leave this process's campaign cache untouched.
    #[serde(default)]
    campaigns: Option<Vec<distill_shell::util::config::CampaignOverride>>,
    #[serde(default)]
    gate_message: Option<String>,
    #[serde(default)]
    gate_url: Option<String>,
    #[serde(default)]
    gate_label: Option<String>,
    #[serde(default)]
    allow_access: Option<bool>,
    #[serde(default)]
    subscription_tier_display: Option<String>,
    #[serde(default)]
    auto_permission_mode_enabled: Option<bool>,
    #[serde(default)]
    prompt_suggestions_enabled: Option<bool>,
    /// Soft-default permission mode.
    /// Omission happens with older shells that predate the field (they can never clear a mode they don't know about).
    /// That version skew is why this is tri-state instead of a plain `Option`.
    #[serde(default, deserialize_with = "deserialize_presence_aware_string")]
    permission_mode: Option<Option<String>>,
    #[serde(default)]
    group_tool_verbs: Option<bool>,
    #[serde(default)]
    collapsed_edit_blocks: Option<bool>,
    #[serde(default)]
    subscription_watch_interval_secs: Option<u64>,
    /// Tolerant for the same reason as the settings response it mirrors.
    /// A malformed gate must not discard the tier, permission mode, and campaigns that arrive with it.
    #[serde(
        default,
        deserialize_with = "distill_shell::util::config::deserialize_tolerant"
    )]
    consent_gate: Option<distill_shell::util::config::ConsentGate>,
}

/// Presence-aware string: omit gives `None` (`#[serde(default)]`), null gives `Some(None)`, and a string gives `Some(Some(_))`.
fn deserialize_presence_aware_string<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

/// Presence-aware and tolerant tags map for live settings updates.
/// Only invoked when the field is present (`#[serde(default)]` covers omit).
/// malformed input warns and gives `Ok(None)` (leave tags alone; do not fail the struct)
fn deserialize_settings_update_tags<'de, D>(
    deserializer: D,
) -> Result<Option<Option<std::collections::BTreeMap<String, String>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(Some(None)),
        v => match serde_json::from_value::<std::collections::BTreeMap<String, String>>(v) {
            Ok(m) => Ok(Some(Some(m))),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "malformed slash_command_tags in settings update; leaving tags unchanged"
                );
                Ok(None)
            }
        },
    }
}

#[cfg(test)]
mod presence_aware_dto_tests {
    use super::*;

    #[derive(Deserialize)]
    struct Probe {
        #[serde(default, deserialize_with = "deserialize_presence_aware_string")]
        permission_mode: Option<Option<String>>,
    }

    #[test]
    fn permission_mode_dto_distinguishes_omit_from_null() {
        let omit: Probe = serde_json::from_value(serde_json::json!({
            "show_resolved_model": true,
        }))
        .unwrap();
        assert_eq!(omit.permission_mode, None, "omit must be None (no update)");

        let null_v: Probe = serde_json::from_value(serde_json::json!({
            "permission_mode": null,
        }))
        .unwrap();
        assert_eq!(
            null_v.permission_mode,
            Some(None),
            "explicit null must be Some(None)"
        );

        let some_v: Probe = serde_json::from_value(serde_json::json!({
            "permission_mode": "always-approve",
        }))
        .unwrap();
        assert_eq!(
            some_v.permission_mode,
            Some(Some("always-approve".into())),
            "string must be Some(Some(_))"
        );
    }

    #[test]
    fn malformed_consent_gate_does_not_discard_the_rest_of_the_update() {
        let update: PagerSettingsUpdate = serde_json::from_value(serde_json::json!({
            "consent_gate": {"version": "not-a-number"},
            "tips": ["still applied"],
        }))
        .expect("a malformed gate must not fail the whole update");

        assert!(update.consent_gate.is_none());
        assert_eq!(
            update.tips.as_deref(),
            Some(["still applied".to_string()].as_slice()),
        );
    }

    #[test]
    fn slash_command_tags_dto_absent_null_map_and_malformed() {
        // 1. Field absent: outer None (leave tags alone)
        let absent: PagerSettingsUpdate = serde_json::from_value(serde_json::json!({
            "tips": ["hello"],
        }))
        .expect("absent slash_command_tags must not fail parse");
        assert_eq!(absent.slash_command_tags, None, "omit must be None");
        assert_eq!(
            absent.tips.as_deref(),
            Some(["hello".to_string()].as_slice())
        );

        // 2. Explicit null: Some(None) (remote cleared)
        let null_v: PagerSettingsUpdate = serde_json::from_value(serde_json::json!({
            "slash_command_tags": null,
        }))
        .expect("null slash_command_tags must parse");
        assert_eq!(
            null_v.slash_command_tags,
            Some(None),
            "explicit null must be Some(None)"
        );

        // 3. Valid map: Some(Some(map))
        let map_v: PagerSettingsUpdate = serde_json::from_value(serde_json::json!({
            "slash_command_tags": {"workflows": "new"},
        }))
        .expect("valid slash_command_tags map must parse");
        let tags = map_v
            .slash_command_tags
            .as_ref()
            .and_then(|inner| inner.as_ref())
            .expect("expected Some(Some(map))");
        assert_eq!(tags.get("workflows").map(String::as_str), Some("new"));
        assert_eq!(tags.len(), 1);

        // 4. Malformed must NOT fail the whole struct; sibling fields still apply
        let bad: PagerSettingsUpdate = serde_json::from_value(serde_json::json!({
            "slash_command_tags": ["oops"],
            "tips": ["still-applied"],
            "permission_mode": "always-approve",
        }))
        .expect("malformed slash_command_tags must not fail PagerSettingsUpdate parse");
        assert_eq!(
            bad.slash_command_tags, None,
            "malformed tags treated as absent"
        );
        assert_eq!(
            bad.tips.as_deref(),
            Some(["still-applied".to_string()].as_slice()),
            "sibling tips must still parse"
        );
        assert_eq!(
            bad.permission_mode,
            Some(Some("always-approve".into())),
            "sibling permission_mode must still parse"
        );
    }
}
