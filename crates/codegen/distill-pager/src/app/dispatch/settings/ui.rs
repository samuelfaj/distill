// Modified for Distill by Samuel Fajreldines, 2026.
//! Settings UI: command palette, settings modal, toggles, resets, and rollback.

use super::setters::{
    pr13_effective_default, set_ask_user_question_timeout_enabled_inner, set_auto_dark_theme_inner,
    set_auto_light_theme_inner, set_auto_update_inner, set_collapsed_edit_blocks_inner,
    set_combine_queued_prompts_inner, set_compact_mode, set_compact_mode_inner,
    set_confirm_before_rewind_inner, set_contextual_hint_inner, set_default_model_inner,
    set_default_selected_permission_inner, set_display_refresh_auto_cadence_inner,
    set_follow_up_behavior_inner, set_fork_secondary_model_inner, set_group_tool_verbs_inner,
    set_hunk_tracker_mode_inner, set_invert_scroll_inner, set_keep_text_selection_inner,
    set_max_thoughts_width_inner, set_multiline_mode, set_page_flip_on_send_inner,
    set_prompt_suggestions_inner, set_remember_tool_approvals_inner, set_render_mermaid_inner,
    set_respect_manual_folds_inner, set_screen_mode_inner, set_scroll_lines_inner,
    set_scroll_mode_inner, set_scroll_speed_inner, set_show_thinking_blocks_inner,
    set_show_tips_inner, set_simple_mode_inner, set_theme_inner, set_timeline_inner,
    set_timestamps, set_timestamps_inner, set_vim_mode_inner, set_voice_capture_mode_inner,
    set_voice_keybind_enabled_inner, set_voice_stt_language_inner,
};
use crate::app::actions::{Action, Effect};
use crate::app::app_view::{ActiveView, AppView};
use crate::app::dispatch::ctx::with_active_agent;
use crate::app::dispatch::modes::{set_yolo_mode_inner, sync_active_auto_flag};
use crate::app::dispatch::router::dispatch;
use crate::app::dispatch::turn::apply_cancel_subagents_preference_global;
use crate::scrollback::block::RenderBlock;
use agent_client_protocol as acp;

/// Format a "✓ Label: value" success toast.
pub(in crate::app::dispatch) fn save_success_toast(label: &str, on: bool) -> String {
    let value = if on { "on" } else { "off" };
    format!("\u{2713} {label}: {value}")
}

/// Refresh every open settings modal's `ui_snapshot` and `pager_snapshot` so the next render reads the latest live state.
/// The modal stores snapshots by value; without this, toggles would appear stuck.
pub(crate) fn refresh_open_settings_modals(app: &mut AppView) {
    use crate::views::modal::ActiveModal;
    // Early exit when no settings modal is open (common case).
    if !app.agents.values().any(|a| {
        matches!(
            a.active_modal,
            Some(ActiveModal::Settings { .. } | ActiveModal::ResetSettingsConfirm { .. })
        )
    }) {
        return;
    }
    let ui_snapshot = app.current_ui.clone();
    // Capture app-level fields before the mut-borrow loop.
    let coding_data_sharing_opt_out_from_app = app.coding_data_retention_opt_out;
    let coding_data_sharing_lock_from_app = app.coding_data_sharing_lock();
    let show_tips_from_app = app.show_tips;
    let auto_update_from_app = app.auto_update;
    let respect_manual_folds_from_app = app.appearance.scrollback.scroll.respect_manual_folds;
    let auto_mode_gate_from_app = app.auto_mode_gate;
    let ask_user_question_timeout_enabled_from_app = app.ask_user_question_timeout_enabled;
    let voice_stt_language_from_app = app.voice_config.language.clone();
    for agent in app.agents.values_mut() {
        // Walk both `Settings` and `ResetSettingsConfirm`
        // The confirm dialog embeds settings state that must stay fresh through async persist failures
        let state_opt = match agent.active_modal.as_mut() {
            Some(ActiveModal::Settings { state }) => Some(state.as_mut()),
            Some(ActiveModal::ResetSettingsConfirm { settings_state, .. }) => {
                Some(settings_state.as_mut())
            }
            _ => None,
        };
        if let Some(state) = state_opt {
            state.rebuild_rows();
            state.ui_snapshot = ui_snapshot.clone();
            state.pager_snapshot = crate::settings::PagerLocalSnapshot {
                multiline_mode: agent.multiline_mode,
                yolo_mode: agent.session.is_yolo(),
                auto_mode: agent.session.is_auto(),
                current_model_name: agent.session.models.current_model_name(),
                available_models: agent
                    .session
                    .models
                    .available
                    .iter()
                    .map(|(id, info)| (info.name.clone(), id.clone()))
                    .collect(),
                coding_data_sharing_opt_out: coding_data_sharing_opt_out_from_app,
                coding_data_sharing_lock: coding_data_sharing_lock_from_app,
                // Prefer optimistic pending over confirmed active.
                plan_mode_active: agent.plan_mode_pending.unwrap_or(agent.plan_mode_active),
                show_tips: show_tips_from_app,
                auto_update: auto_update_from_app,
                vim_mode: crate::appearance::cache::load_vim_mode(),
                scroll_speed: crate::appearance::cache::load_scroll_speed(),
                respect_manual_folds: respect_manual_folds_from_app,
                auto_mode_gate: auto_mode_gate_from_app,
                ask_user_question_timeout_enabled: ask_user_question_timeout_enabled_from_app,
                voice_stt_language: voice_stt_language_from_app.clone(),
            };
        }
    }
}

/// Open the command palette (the `/help` slash command path).
/// Mirrors the keybinding handler (`ActionId::CommandPalette`); toggles closed if already open.
/// In minimal mode the overlay app-modal host renders it inline.
pub(in crate::app::dispatch) fn dispatch_open_command_palette(app: &mut AppView) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if matches!(
        &agent.active_modal,
        Some(ActiveModal::CommandPalette { .. })
    ) {
        agent.active_modal = None;
        return vec![];
    }
    agent.active_modal = Some(ActiveModal::CommandPalette {
        entries: crate::views::modal::default_palette_entries(
            agent.sharing_enabled,
            &agent.prompt.slash_controller,
        ),
        // Type-to-find: open in input mode (matches Ctrl+P).
        state: crate::views::picker::PickerState::input_active(),
        window: crate::views::modal_window::ModalWindowState::new(),
    });
    vec![]
}

/// Open the How-to Guides doc picker (`/docs`). Toggles closed if already open.
pub(in crate::app::dispatch) fn dispatch_open_howto_guides(app: &mut AppView) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if matches!(&agent.active_modal, Some(ActiveModal::DocPicker { .. })) {
        agent.active_modal = None;
        return vec![];
    }
    agent.active_modal = Some(crate::views::modal::howto_list_modal(None));
    vec![]
}

/// Open the settings modal. Reads the live `UiConfig` snapshot (sans-IO).
/// Only one settings modal can be open; `debug_assert!` catches routing bugs.
/// When not on an agent view, switches to an existing agent or creates a placeholder session so the modal can mount.
pub(in crate::app::dispatch) fn dispatch_open_settings(
    app: &mut AppView,
    focus_key: Option<&'static str>,
) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;
    use crate::views::settings_modal::{RowVisibility, SettingsModalState};

    let mut effects = vec![];
    let id = match app.active_view {
        ActiveView::Agent(id) => id,
        _ => {
            if let Some(existing) = app.agents.keys().next().copied() {
                crate::app::dispatch::ctx::switch_to_agent(
                    app,
                    existing,
                    crate::app::dispatch::ctx::SwitchCause::Picker,
                );
                existing
            } else {
                let (new_id, create_effects) =
                    crate::app::dispatch::session::lifecycle::dispatch_new_session_inner_with_id(
                        app, None, false,
                    );
                effects.extend(create_effects);
                new_id
            }
        }
    };
    // Snapshot the registry, UiConfig, and pager-local state before the mutable borrow on `agent` so the borrow checker is happy
    let registry = app.settings_registry.clone();
    let ui_snapshot = app.current_ui.clone();
    // Capture app-level fields before the mut-borrow on the agent.
    let coding_data_sharing_opt_out_from_app = app.coding_data_retention_opt_out;
    let coding_data_sharing_lock_from_app = app.coding_data_sharing_lock();
    let show_tips_from_app = app.show_tips;
    let auto_update_from_app = app.auto_update;
    let respect_manual_folds_from_app = app.appearance.scrollback.scroll.respect_manual_folds;
    let auto_mode_gate_from_app = app.auto_mode_gate;
    let ask_user_question_timeout_enabled_from_app = app.ask_user_question_timeout_enabled;
    let voice_stt_language_from_app = app.voice_config.language.clone();
    // Theme rows are `hidden_in_minimal`. Snapshot this AppView's mode, not `MINIMAL_MODE_ACTIVE`
    // (other tests flip that process flag in parallel and would drop `theme` from the list).
    let visibility = RowVisibility {
        hide_appearance: app.screen_mode.is_minimal(),
    };

    let Some(agent) = app.agents.get_mut(&id) else {
        return effects;
    };

    if matches!(&agent.active_modal, Some(ActiveModal::Settings { .. })) {
        if focus_key.is_none() {
            debug_assert!(
                false,
                "OpenSettings dispatched while settings modal is already open — input routing bug"
            );
            // Defensive close in release builds: a silent no-op is worse than one extra branch here
            // Mirrors the same guard in `views/shortcuts_help.rs`
            agent.active_modal = None;
            return effects;
        }
        agent.active_modal = None;
    }

    tracing::info!(target: "settings", "opened modal");

    let pager_snapshot = crate::settings::PagerLocalSnapshot {
        multiline_mode: agent.multiline_mode,
        yolo_mode: agent.session.is_yolo(),
        auto_mode: agent.session.is_auto(),
        current_model_name: agent.session.models.current_model_name(),
        available_models: agent
            .session
            .models
            .available
            .iter()
            .map(|(id, info)| (info.name.clone(), id.clone()))
            .collect(),
        coding_data_sharing_opt_out: coding_data_sharing_opt_out_from_app,
        coding_data_sharing_lock: coding_data_sharing_lock_from_app,
        // Prefer optimistic pending over confirmed active.
        plan_mode_active: agent.plan_mode_pending.unwrap_or(agent.plan_mode_active),
        show_tips: show_tips_from_app,
        auto_update: auto_update_from_app,
        vim_mode: crate::appearance::cache::load_vim_mode(),
        scroll_speed: crate::appearance::cache::load_scroll_speed(),
        respect_manual_folds: respect_manual_folds_from_app,
        auto_mode_gate: auto_mode_gate_from_app,
        ask_user_question_timeout_enabled: ask_user_question_timeout_enabled_from_app,
        voice_stt_language: voice_stt_language_from_app,
    };
    let mut state = Box::new(SettingsModalState::new_with_row_visibility(
        registry,
        ui_snapshot,
        pager_snapshot,
        visibility,
    ));
    if let Some(key) = focus_key
        && state.focus_key(key)
    {
        // Try the chooser; a locked row keeps Browse (`try_enter_picking_enum` refuses when `row_lock` is set)
        if state.try_enter_picking_enum() {
            state.close_on_picker_exit = true;
        }
    }
    agent.active_modal = Some(ActiveModal::Settings { state });
    effects
}

/// Open the reset-settings confirmation modal.
/// Only the settings modal's `d` key (`views/settings_modal.rs::handle_browse`) emits this, so any other active modal is a routing bug.
/// `debug_assert!` catches it in debug builds; release degrades to a no-op instead of crashing.
pub(in crate::app::dispatch) fn dispatch_open_reset_confirm(
    app: &mut AppView,
    key: crate::settings::SettingKey,
) -> Vec<Effect> {
    use crate::views::modal::{ActiveModal, ModalConfirmation};

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    // Take the Settings modal state out so it can move into the confirmation variant
    // If the active modal is anything else (routing bug), restore the original modal and bail
    // The debug_assert surfaces the bug in tests; release builds degrade to a silent no-op rather than a panic
    let prior = agent.active_modal.take();
    let settings_state = match prior {
        Some(ActiveModal::Settings { state }) => state,
        other => {
            debug_assert!(
                matches!(other, Some(ActiveModal::Settings { .. })),
                "OpenResetConfirm dispatched without an open Settings modal — input routing bug",
            );
            agent.active_modal = other;
            return vec![];
        }
    };

    // debug! level: per-keystroke logging would flood production
    tracing::debug!(target: "settings", key, "opened reset-confirm modal");
    agent.active_modal = Some(ActiveModal::ResetSettingsConfirm {
        modal: ModalConfirmation::reset_settings(),
        key,
        settings_state,
    });
    vec![]
}

/// Reset looks up the registered default and shows an "Already at default" toast when the value would not change.
/// Otherwise it maps the default to the typed `Action::SetX(default)` via `action_for_reset` and dispatches it recursively.
/// The recursive dispatch runs the full setter pipeline (persist, toast, snapshot refresh) and is at most 2 frames deep.
pub(in crate::app::dispatch) fn dispatch_confirm_reset_setting(
    app: &mut AppView,
    choice: crate::views::modal::ResetSettingsResult,
) -> Vec<Effect> {
    use crate::views::modal::{ActiveModal, ResetSettingsResult};

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    // Take the ResetSettingsConfirm modal out, capturing the target key from the variant
    // The variant is the single source of truth: the Action does not carry the key, so a future emitter cannot desync it
    let prior = agent.active_modal.take();
    let (key, settings_state) = match prior {
        Some(ActiveModal::ResetSettingsConfirm {
            key,
            settings_state,
            ..
        }) => (key, settings_state),
        other => {
            debug_assert!(
                matches!(other, Some(ActiveModal::ResetSettingsConfirm { .. })),
                "ConfirmResetSetting dispatched without an open ResetSettingsConfirm modal — \
                 input routing bug",
            );
            agent.active_modal = other;
            return vec![];
        }
    };
    agent.active_modal = Some(ActiveModal::Settings {
        state: settings_state,
    });

    match choice {
        ResetSettingsResult::Cancel => {
            tracing::debug!(target: "settings", key, "reset confirmation cancelled");
            vec![]
        }
        ResetSettingsResult::Reset => {
            let registry_arc = app.settings_registry.clone();
            let Some(meta) = registry_arc.find(key) else {
                tracing::error!(
                    target: "settings",
                    key,
                    "reset target points at unregistered key — registry/dispatch skew",
                );
                return vec![];
            };
            let default_value = crate::settings::default_value_for(meta);
            let Some(action) = action_for_reset(key, &default_value) else {
                tracing::error!(
                    target: "settings",
                    key,
                    ?default_value,
                    "reset has no action_for_reset arm — registry/dispatch skew",
                );
                return vec![];
            };

            // Gate idempotent reset: a value already at its default only shows a toast
            // Not for the coding-data setter, which owns that decision: its local "opt-out" may be the unconfirmed fail-safe, so it writes anyway
            let pager_snapshot = build_pager_snapshot(app);
            let current_value =
                crate::settings::current_value_for(key, &app.current_ui, &pager_snapshot);
            if current_value.as_ref() == Some(&default_value)
                && !matches!(action, Action::SetCodingDataSharing { .. })
            {
                tracing::debug!(
                    target: "settings",
                    key,
                    ?default_value,
                    "reset skipped — setting already at default",
                );
                with_active_agent(app, |agent| {
                    agent.show_toast(&format!("{}: already at default", meta.label));
                });
                return vec![];
            }

            tracing::info!(
                target: "settings",
                key,
                ?default_value,
                "resetting setting to default",
            );
            // Recursive dispatch runs the full set_X pipeline (see the doc comment above); max depth is 2 frames
            dispatch(action, app)
        }
    }
}

/// Toggle multiline input mode (Ctrl+M keybinding path).
/// Delegates to `set_multiline_mode`; the mode is pager-owned, per-agent, and never persisted.
pub(in crate::app::dispatch) fn dispatch_toggle_multiline(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get(&id) else {
        return vec![];
    };
    let new = !agent.multiline_mode;
    set_multiline_mode(app, new)
}

/// Toggle compact mode (keybinding path).
/// Delegates to the registry-driven `set_compact_mode` so the cache, modal snapshot, and `Effect::PersistSetting` all flow through one path.
pub(in crate::app::dispatch) fn dispatch_toggle_compact_mode(app: &mut AppView) -> Vec<Effect> {
    // Toggle the user value: `appearance.prompt.compact` is the derived render value, which auto-compact forces on short terminals regardless of it
    let new = !app.current_ui.compact_mode;
    set_compact_mode(app, new)
}

/// Toggle vim-style scrollback keybindings (`/vim-mode` slash command path).
/// Off is the default: bare-letter and Shift+letter scrollback bindings (j/k/h/l/g/G/y/Y/o/O/r/x/e/E/L/H and the `i` FocusPrompt alt) are suppressed.
/// Delegates to the registry-driven `set_vim_mode` so the cache, modal snapshot, toast, and `Effect::PersistSetting` all flow through one path.
pub(in crate::app::dispatch) fn dispatch_toggle_vim_mode(app: &mut AppView) -> Vec<Effect> {
    // Toggle the effective value (the pager cache) so `/vim-mode` works from any view, including the session-less dashboard
    let prev = crate::appearance::cache::load_vim_mode();
    let enabled = !prev;
    // Propagate to every agent and nested subagent view and mirror the pager cache
    // Background and open subagent views pick up the change without a restart; `set_vim_mode_inner` is shared with the `SetVimMode` settings path
    set_vim_mode_inner(app, enabled);
    refresh_open_settings_modals(app);
    let msg = if enabled {
        "Vim mode: on"
    } else {
        "Vim mode: off"
    };
    tracing::info!(vim_mode = enabled, "Vim mode toggled");
    match app.active_view {
        ActiveView::Agent(id) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent
                    .scrollback
                    .push_block(RenderBlock::system(msg.to_string()));
            }
        }
        ActiveView::AgentDashboard => {
            // On the dashboard, j/k navigate the overview only when it holds focus
            // Turning vim on focuses the overview so the user can navigate immediately, mirroring the agent view's "normal mode"
            // A toast would route to the dashboard's red error slot
            let has_agents = !app.agents.is_empty();
            if let Some(d) = app.dashboard.as_mut() {
                d.list_focused = enabled && has_agents;
            }
        }
        _ => {
            app.show_toast(msg);
        }
    }
    // Persist like the shared setter so `/vim-mode` survives a restart (writes `[ui].vim_mode` to config.toml)
    vec![Effect::PersistSetting {
        key: "vim_mode",
        value: crate::settings::SettingValue::Bool(enabled),
        rollback_value: crate::settings::SettingValue::Bool(prev),
    }]
}

/// Toggle timestamps (Ctrl+? keybinding path).
/// Delegates to the registry-driven `set_timestamps` so persistence, the cache, and UI reconciliation all flow through a single code path.
pub(in crate::app::dispatch) fn dispatch_toggle_timestamps(app: &mut AppView) -> Vec<Effect> {
    let show = !app.appearance.show_timestamps;
    set_timestamps(app, show)
}

/// Toggle terminal mouse reporting (crossterm mouse capture) at the user's discretion.
/// Disabling it lets the terminal handle native click-drag text selection and copy/paste.
/// Re-enabling restores in-app mouse handling (click-to-focus, scrollback selection, scrollbar drag, etc.).
pub(in crate::app::dispatch) fn dispatch_toggle_mouse_capture(app: &mut AppView) {
    use std::sync::atomic::Ordering;

    // User took ownership; do not restore our previous hold when the native-select surface closes.
    app.native_select_hold = false;
    let was_enabled = crate::app::MOUSE_CAPTURE_ENABLED.load(Ordering::Acquire);
    let enable = !was_enabled;
    crate::unified_log::info(
        "mouse_reporting_toggle.toggle",
        None,
        Some(serde_json::json!({
            "was_enabled": was_enabled,
            "now_enabled": enable,
            "active_view": format!("{:?}", app.active_view),
        })),
    );
    // Via the writer queue, never inline (event-loop thread; see EscapeWriter)
    if enable {
        app.escape_writer
            .emit_command(crossterm::event::EnableMouseCapture);
    } else {
        app.escape_writer
            .emit_command(crossterm::event::DisableMouseCapture);
    }
    // On legacy conhost, DisableMouseCapture restores the *pre-capture* stdin mode
    // That mode may itself have QuickEdit off (a per-window profile or a stale mode from a crashed run)
    // Assert it so "mouse off" actually hands the terminal native drag-select, the whole point of the toggle
    #[cfg(windows)]
    if !enable {
        crate::app::win_native_selection::enable_native_selection();
    }
    crate::app::MOUSE_CAPTURE_ENABLED.store(enable, Ordering::Release);
    // Use with_active_agent (not app.show_toast) so the toast lands on the view the user is looking at
    // Off state: sticky banner on every agent/subagent view (capture is process-wide; the toast must survive subagent open/close and copy toasts)
    // Ctrl+R only re-enables from scrollback
    let mut toast_applied = false;
    if enable {
        for agent in app.agents.values_mut() {
            agent.set_sticky_toast_recursive(None);
        }
        with_active_agent(app, |agent| {
            toast_applied = true;
            agent.show_toast("Mouse reporting on");
        });
    } else {
        for agent in app.agents.values_mut() {
            agent.set_sticky_toast_recursive(Some(crate::app::MOUSE_OFF_HINT_SCROLLBACK));
            toast_applied = true;
        }
    }
    crate::unified_log::info(
        "mouse_reporting_toggle.toggle_done",
        None,
        Some(serde_json::json!({
            "now_enabled": enable,
            "toast_applied": toast_applied,
        })),
    );
}

/// Read the active agent's `multiline_mode` for the pager-local snapshot used in idempotent-reset detection.
/// Returns `false` with no active agent: no Settings modal is open in that state, so the value does not matter.
fn agent_multiline_mode(app: &AppView) -> bool {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get(&id)
    {
        return agent.multiline_mode;
    }
    false
}

/// Read the active agent's `yolo_mode`. See [`agent_multiline_mode`] for the no-agent fallback rationale.
fn agent_yolo_mode(app: &AppView) -> bool {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get(&id)
    {
        return agent.session.is_yolo();
    }
    false
}

/// Read the active agent's `auto_mode`. See [`agent_multiline_mode`] for the no-agent fallback rationale.
fn agent_auto_mode(app: &AppView) -> bool {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get(&id)
    {
        return agent.session.is_auto();
    }
    false
}

/// Effective `plan_mode` for the active agent (`pending.unwrap_or(active)`).
fn agent_plan_mode(app: &AppView) -> bool {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get(&id)
    {
        return agent.plan_mode_pending.unwrap_or(agent.plan_mode_active);
    }
    false
}

/// Read the active agent's currently-selected model display name; the `default_model` row's `current_value_for` uses it.
/// Returns `None` when no agent is active or the catalog hasn't loaded yet (e.g. early startup).
/// See [`agent_multiline_mode`] for the no-agent fallback rationale.
fn agent_current_model_name(app: &AppView) -> Option<String> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get(&id)
    {
        return agent.session.models.current_model_name();
    }
    None
}

/// Clone the `(display_name, ModelId)` pairs from the active agent's catalog.
/// Returns an empty `Vec` when no agent is active or the catalog is empty.
/// The `KnownModel` validator and the "resolve user input to ModelId" path use it.
fn agent_available_models(app: &AppView) -> Vec<(String, acp::ModelId)> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get(&id)
    {
        return agent
            .session
            .models
            .available
            .iter()
            .map(|(id, info)| (info.name.clone(), id.clone()))
            .collect();
    }
    Vec::new()
}

pub(crate) fn build_pager_snapshot(app: &AppView) -> crate::settings::PagerLocalSnapshot {
    crate::settings::PagerLocalSnapshot {
        multiline_mode: agent_multiline_mode(app),
        yolo_mode: agent_yolo_mode(app),
        auto_mode: agent_auto_mode(app),
        current_model_name: agent_current_model_name(app),
        available_models: agent_available_models(app),
        coding_data_sharing_opt_out: app.coding_data_retention_opt_out,
        coding_data_sharing_lock: app.coding_data_sharing_lock(),
        plan_mode_active: agent_plan_mode(app),
        show_tips: app.show_tips,
        auto_update: app.auto_update,
        vim_mode: crate::appearance::cache::load_vim_mode(),
        scroll_speed: crate::appearance::cache::load_scroll_speed(),
        respect_manual_folds: app.appearance.scrollback.scroll.respect_manual_folds,
        auto_mode_gate: app.auto_mode_gate,
        ask_user_question_timeout_enabled: app.ask_user_question_timeout_enabled,
        voice_stt_language: app.voice_config.language.clone(),
    }
}

/// Map a `(SettingKey, SettingValue)` to the matching typed `Action::SetX(value)`.
/// The reset path uses it to turn the registered default into a dispatchable Action.
/// The CI test `every_setting_has_action_for_reset_arm` pins that every setting has an arm.
pub(in crate::app::dispatch) fn action_for_reset(
    key: crate::settings::SettingKey,
    value: &crate::settings::SettingValue,
) -> Option<Action> {
    use crate::settings::SettingValue;
    match (key, value) {
        ("compact_mode", SettingValue::Bool(b)) => Some(Action::SetCompactMode(*b)),
        ("show_timestamps", SettingValue::Bool(b)) => Some(Action::SetTimestamps(*b)),
        ("show_timeline", SettingValue::Bool(b)) => Some(Action::SetTimeline(*b)),
        ("page_flip_on_send", SettingValue::Bool(b)) => Some(Action::SetPageFlipOnSend(*b)),
        ("confirm_before_rewind", SettingValue::Bool(b)) => {
            Some(Action::SetConfirmBeforeRewind(*b))
        }
        ("combine_queued_prompts", SettingValue::Bool(b)) => {
            Some(Action::SetCombineQueuedPrompts(*b))
        }
        ("follow_up_behavior", SettingValue::Enum(s)) => {
            crate::appearance::FollowUpBehavior::from_canonical(s).map(Action::SetFollowUpBehavior)
        }
        ("simple_mode", SettingValue::Bool(b)) => Some(Action::SetSimpleMode(*b)),
        ("contextual_hints.undo", SettingValue::Bool(b)) => Some(Action::SetContextualHintUndo(*b)),
        ("contextual_hints.plan_mode", SettingValue::Bool(b)) => {
            Some(Action::SetContextualHintPlanMode(*b))
        }
        ("contextual_hints.image_input", SettingValue::Bool(b)) => {
            Some(Action::SetContextualHintImageInput(*b))
        }
        ("contextual_hints.send_now", SettingValue::Bool(b)) => {
            Some(Action::SetContextualHintSendNow(*b))
        }
        ("contextual_hints.small_screen", SettingValue::Bool(b)) => {
            Some(Action::SetContextualHintSmallScreen(*b))
        }
        ("contextual_hints.word_select", SettingValue::Bool(b)) => {
            Some(Action::SetContextualHintWordSelect(*b))
        }
        ("contextual_hints.export_copy", SettingValue::Bool(b)) => {
            Some(Action::SetContextualHintExportCopy(*b))
        }
        ("contextual_hints.ssh_wrap", SettingValue::Bool(b)) => {
            Some(Action::SetContextualHintSshWrap(*b))
        }
        ("multiline_mode", SettingValue::Bool(b)) => Some(Action::SetMultilineMode(*b)),
        ("render_mermaid", SettingValue::Enum(s)) => {
            crate::appearance::RenderMermaid::from_canonical(s).map(Action::SetRenderMermaid)
        }
        ("vim_mode", SettingValue::Bool(b)) => Some(Action::SetVimMode(*b)),
        ("remember_tool_approvals", SettingValue::Bool(b)) => {
            Some(Action::SetRememberToolApprovals(*b))
        }
        ("toolset.ask_user_question.timeout_enabled", SettingValue::Bool(b)) => {
            Some(Action::SetAskUserQuestionTimeoutEnabled(*b))
        }
        ("keep_text_selection", SettingValue::Enum(s)) => {
            crate::appearance::TextSelection::from_canonical(s).map(Action::SetKeepTextSelection)
        }
        ("scroll_speed", SettingValue::Int(v)) => Some(Action::SetScrollSpeed(*v)),
        ("scroll_mode", SettingValue::Enum(s)) => {
            crate::appearance::ScrollMode::from_canonical(s).map(Action::SetScrollMode)
        }
        ("invert_scroll", SettingValue::Bool(b)) => Some(Action::SetInvertScroll(*b)),
        ("scroll_lines", SettingValue::Int(v)) => Some(Action::SetScrollLines(*v)),
        ("show_thinking_blocks", SettingValue::Bool(b)) => Some(Action::SetShowThinkingBlocks(*b)),
        ("group_tool_verbs", SettingValue::Bool(b)) => Some(Action::SetGroupToolVerbs(*b)),
        ("collapsed_edit_blocks", SettingValue::Bool(b)) => {
            Some(Action::SetCollapsedEditBlocks(*b))
        }
        ("prompt_suggestions", SettingValue::Bool(b)) => Some(Action::SetPromptSuggestions(*b)),
        ("respect_manual_folds", SettingValue::Bool(b)) => Some(Action::SetRespectManualFolds(*b)),
        ("default_selected_permission", SettingValue::Enum(s)) => {
            Some(Action::SetDefaultSelectedPermission((*s).to_owned()))
        }
        ("theme", SettingValue::Enum(s)) => Some(Action::SetTheme((*s).to_owned())),
        ("auto_dark_theme", SettingValue::Enum(s)) => {
            Some(Action::SetAutoDarkTheme((*s).to_owned()))
        }
        ("auto_light_theme", SettingValue::Enum(s)) => {
            Some(Action::SetAutoLightTheme((*s).to_owned()))
        }
        // One arm per canonical, all dispatched through the typed `Action::SetPermissionMode(kind)`, not the legacy `Action::SetYoloMode(bool)`
        // Only that arm is reachable through the normal reset flow
        // The other arms are there so a changed registered default fires its matching arm rather than silently collapsing onto SetYoloMode
        ("permission_mode", SettingValue::Enum("always-approve")) => Some(
            Action::SetPermissionMode(crate::app::actions::PermissionModeKind::AlwaysApprove),
        ),
        ("permission_mode", SettingValue::Enum("ask")) => Some(Action::SetPermissionMode(
            crate::app::actions::PermissionModeKind::Ask,
        )),
        ("permission_mode", SettingValue::Enum("auto")) => Some(Action::SetPermissionMode(
            crate::app::actions::PermissionModeKind::Auto,
        )),
        ("permission_mode", SettingValue::Enum("default")) => Some(Action::SetPermissionMode(
            crate::app::actions::PermissionModeKind::Default,
        )),
        // default_model: an empty string becomes ClearDefaultModel; non-empty is a registry/dispatch skew guard
        ("default_model", SettingValue::String(s)) => {
            if s.is_empty() {
                Some(Action::ClearDefaultModel)
            } else {
                tracing::error!(
                    target: "settings",
                    value = %s,
                    "action_for_reset(default_model) received non-empty default — \
                     registry/dispatch skew (default should be empty string)",
                );
                None
            }
        }
        // max_thoughts_width: direct round-trip.
        ("max_thoughts_width", SettingValue::Int(i)) => Some(Action::SetMaxThoughtsWidth(*i)),
        // coding_data_sharing: "opt-in" / "opt-out" map to bool; both arms are needed (registry default is "opt-out")
        ("coding_data_sharing", SettingValue::Enum("opt-in")) => {
            Some(Action::SetCodingDataSharing { opted_in: true })
        }
        ("coding_data_sharing", SettingValue::Enum("opt-out")) => {
            Some(Action::SetCodingDataSharing { opted_in: false })
        }
        // plan_mode: "on" / "off" map to PlanModeKind; the "on" arm is a skew guard (default is "off")
        ("plan_mode", SettingValue::Enum("off")) => {
            Some(Action::SetPlanMode(crate::app::actions::PlanModeKind::Off))
        }
        ("plan_mode", SettingValue::Enum("on")) => {
            Some(Action::SetPlanMode(crate::app::actions::PlanModeKind::On))
        }
        // show_tips / auto_update / display_refresh_auto_cadence: direct bool.
        ("show_tips", SettingValue::Bool(b)) => Some(Action::SetShowTips(*b)),
        ("auto_update", SettingValue::Bool(b)) => Some(Action::SetAutoUpdate(*b)),
        ("display_refresh_auto_cadence", SettingValue::Bool(b)) => {
            Some(Action::SetDisplayRefreshAutoCadence(*b))
        }
        // hunk_tracker_mode: canonical enum string round-trip.
        ("hunk_tracker_mode", SettingValue::Enum(s)) => {
            Some(Action::SetHunkTrackerMode((*s).to_string()))
        }
        ("screen_mode", SettingValue::Enum(s)) => Some(Action::SetScreenMode((*s).to_string())),
        ("voice_keybind_enabled", SettingValue::Bool(b)) => {
            Some(Action::SetVoiceKeybindEnabled(*b))
        }
        ("voice_capture_mode", SettingValue::Enum(s)) => {
            Some(Action::SetVoiceCaptureMode((*s).to_string()))
        }
        ("voice_stt_language", SettingValue::Enum(s)) => {
            Some(Action::SetVoiceSttLanguage((*s).to_string()))
        }
        // fork_secondary_model: empty becomes Clear, non-empty is a skew guard
        ("fork_secondary_model", SettingValue::String(s)) => {
            if s.is_empty() {
                Some(Action::ClearForkSecondaryModel)
            } else {
                tracing::error!(
                    target: "settings",
                    value = %s,
                    "action_for_reset(fork_secondary_model) received non-empty default — \
                     registry/dispatch skew (default should be empty string)",
                );
                None
            }
        }

        _ => None,
    }
}

/// Toast shown by the [`apply_setting_rollback`] catch-all when a persisted setting has no rollback arm.
/// Shared with `every_persisting_setting_has_rollback_arm` so the guard can't be silently defeated by a wording edit.
pub(crate) const ROLLBACK_NO_ARM_TOAST: &str =
    "Settings rolled back, but local state may be out of sync: restart to reload";

/// Does not re-emit `PersistSetting` (that would loop forever on a persistent failure); can emit companion effects (e.g. a reverse `SwitchModel`).
/// New settings must add an arm here.
/// Unknown keys log `error!`, so the inconsistency between the in-memory cache and disk is visible to the user.
pub(in crate::app::dispatch) fn apply_setting_rollback(
    app: &mut AppView,
    key: crate::settings::SettingKey,
    rollback_value: &crate::settings::SettingValue,
) -> Vec<Effect> {
    use crate::settings::SettingValue;
    let mut companion_effects: Vec<Effect> = Vec::new();
    match (key, rollback_value) {
        ("compact_mode", SettingValue::Bool(b)) => set_compact_mode_inner(app, *b),
        ("show_timestamps", SettingValue::Bool(b)) => set_timestamps_inner(app, *b),
        ("show_timeline", SettingValue::Bool(b)) => set_timeline_inner(app, *b),
        ("page_flip_on_send", SettingValue::Bool(b)) => set_page_flip_on_send_inner(app, *b),
        ("confirm_before_rewind", SettingValue::Bool(b)) => {
            set_confirm_before_rewind_inner(app, *b)
        }
        ("combine_queued_prompts", SettingValue::Bool(b)) => {
            set_combine_queued_prompts_inner(app, *b)
        }
        ("follow_up_behavior", SettingValue::Enum(s)) => {
            if let Some(mode) = crate::appearance::FollowUpBehavior::from_canonical(s) {
                set_follow_up_behavior_inner(app, mode);
            }
        }
        ("simple_mode", SettingValue::Bool(b)) => set_simple_mode_inner(app, *b),
        ("contextual_hints.undo", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.undo = v, *b)
        }
        ("contextual_hints.plan_mode", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.plan_mode = v, *b)
        }
        ("contextual_hints.image_input", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.image_input = v, *b)
        }
        ("contextual_hints.send_now", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.send_now = v, *b)
        }
        ("contextual_hints.small_screen", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.small_screen = v, *b)
        }
        ("contextual_hints.word_select", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.word_select = v, *b)
        }
        ("contextual_hints.export_copy", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.export_copy = v, *b)
        }
        ("contextual_hints.ssh_wrap", SettingValue::Bool(b)) => {
            set_contextual_hint_inner(app, |h, v| h.ssh_wrap = v, *b)
        }
        ("respect_manual_folds", SettingValue::Bool(b)) => set_respect_manual_folds_inner(app, *b),
        ("theme", SettingValue::Enum(s)) => set_theme_inner(app, s),
        ("default_selected_permission", SettingValue::Enum(s)) => {
            set_default_selected_permission_inner(
                app,
                crate::appearance::permission_cursor::DefaultSelectedPermission::from_config_value(
                    s,
                ),
            )
        }
        ("cancel_subagents_on_turn_cancel", SettingValue::Enum("ask")) => {
            app.current_ui.cancel_subagents_on_turn_cancel = None;
            for agent in app.agents.values_mut() {
                agent.cancel_subagents_preference = None;
            }
        }
        ("cancel_subagents_on_turn_cancel", SettingValue::Enum("always_stop")) => {
            apply_cancel_subagents_preference_global(app, true);
        }
        ("cancel_subagents_on_turn_cancel", SettingValue::Enum("always_continue")) => {
            apply_cancel_subagents_preference_global(app, false);
        }
        // Rollback for a corrupted auto-* value of "auto": clear to None
        ("auto_dark_theme", SettingValue::Enum("auto")) => {
            app.current_ui.auto_dark_theme = None;
            crate::theme::cache::invalidate_auto_theme_config();
            tracing::warn!(
                target: "settings",
                key = "auto_dark_theme",
                "rolled back to `auto` (invalid) — cleared in-memory override to None",
            );
        }
        ("auto_light_theme", SettingValue::Enum("auto")) => {
            app.current_ui.auto_light_theme = None;
            crate::theme::cache::invalidate_auto_theme_config();
            tracing::warn!(
                target: "settings",
                key = "auto_light_theme",
                "rolled back to `auto` (invalid) — cleared in-memory override to None",
            );
        }
        ("auto_dark_theme", SettingValue::Enum(s)) => set_auto_dark_theme_inner(app, s),
        ("auto_light_theme", SettingValue::Enum(s)) => set_auto_light_theme_inner(app, s),
        // permission_mode rollback: recover the kind from the canonical, run the inner, then restore the canonical the inner collapsed
        ("permission_mode", SettingValue::Enum(s)) => {
            use crate::app::actions::PermissionModeKind;
            let kind = match PermissionModeKind::from_canonical(s) {
                Some(k) => k,
                None => {
                    tracing::warn!(
                        target: "settings",
                        key = "permission_mode",
                        value = *s,
                        "rollback received unknown canonical — defaulting to `ask`",
                    );
                    PermissionModeKind::Ask
                }
            };
            set_yolo_mode_inner(app, kind.is_always_approve());
            // Restore the canonical (meaningful for `Default`)
            app.current_ui.permission_mode = Some(kind.as_canonical().to_string());
            // Sync the per-session auto flag only for a permission_mode rollback
            // Other rollback arms must not clobber it from the global canonical
            sync_active_auto_flag(app);
        }
        // default_model: best-effort rollback. If the prior model no longer resolves, leave the optimistic value and log.
        ("default_model", SettingValue::String(s)) => {
            if s.is_empty() {
                tracing::warn!(
                    target: "settings",
                    key = "default_model",
                    "rollback to empty string requested but no \
                     'clear current model' API exists — leaving live \
                     state at optimistic value (next session reload \
                     will resolve via shell default-resolution chain)",
                );
            } else {
                // Resolve the prior model ID back to a ModelId and call the typed inner
                // If resolution fails (the catalog changed mid-flight), log and leave the optimistic value
                let (resolved, session_id) = if let ActiveView::Agent(aid) = app.active_view
                    && let Some(agent) = app.agents.get(&aid)
                {
                    (
                        agent.session.models.resolve_by_name_or_id(s),
                        agent.session.session_id.clone(),
                    )
                } else {
                    (None, None)
                };
                match resolved {
                    Some(id) => {
                        let _ = set_default_model_inner(app, &id);
                        // Emit a reverse SwitchModel so the ACP session matches the rolled-back pager mirror
                        if let ActiveView::Agent(aid) = app.active_view
                            && let Some(sid) = session_id
                        {
                            if let Some(agent) = app.agents.get_mut(&aid) {
                                agent.session.model_switch_pending = true;
                            }
                            companion_effects.push(Effect::SwitchModel {
                                agent_id: aid,
                                session_id: sid,
                                model_id: id,
                                effort: None,
                                prev_model_id: None,
                            });
                        }
                    }
                    None => {
                        tracing::warn!(
                            target: "settings",
                            key = "default_model",
                            value = %s,
                            "rollback model id no longer resolves in catalog — \
                             in-memory state stays at optimistic value; ACP session \
                             may diverge from pager mirror until next setter dispatch",
                        );
                    }
                }
            }
        }
        // reasoning_model: restore only the mirror; the main model never changed.
        ("reasoning_model", SettingValue::String(s)) => {
            let previous = if s.is_empty() { None } else { Some(acp::ModelId::new(s.clone())) };
            app.models.reasoning_model = previous.clone();
            for agent in app.agents.values_mut() {
                agent.session.models.reasoning_model = previous.clone();
            }
            refresh_open_settings_modals(app);
        }
        // max_thoughts_width: direct inner call.
        ("max_thoughts_width", SettingValue::Int(i)) => set_max_thoughts_width_inner(app, *i),
        // scroll_speed: direct inner call (clamp handled by inner).
        ("scroll_speed", SettingValue::Int(i)) => set_scroll_speed_inner(app, *i as u8),
        // scroll_mode: restore the cache mirror to the canonical value.
        ("scroll_mode", SettingValue::Enum(s)) => {
            if let Some(mode) = crate::appearance::ScrollMode::from_canonical(s) {
                set_scroll_mode_inner(app, mode);
            }
        }
        // No pager-side mirror to roll back; the failure toast is the whole story.
        ("trace_upload", SettingValue::Bool(_)) => {}
        // The in-session suppression latch deliberately stays set even when the disk write fails
        ("feedback_trace_card", SettingValue::Bool(_)) => {}
        // invert_scroll / scroll_lines: direct inner calls (clamp in inner).
        ("invert_scroll", SettingValue::Bool(b)) => set_invert_scroll_inner(app, *b),
        // The effective default is false, so a false rollback restores None (the mirror stays disk-synced)
        ("display_refresh_auto_cadence", SettingValue::Bool(b)) => {
            if !*b {
                app.current_ui.display_refresh.auto_cadence_enabled = None;
            } else {
                set_display_refresh_auto_cadence_inner(app, *b);
            }
        }
        ("scroll_lines", SettingValue::Int(i)) => set_scroll_lines_inner(app, *i as u8),
        // vim_mode: direct inner call.
        ("vim_mode", SettingValue::Bool(b)) => set_vim_mode_inner(app, *b),
        ("remember_tool_approvals", SettingValue::Bool(b)) => {
            set_remember_tool_approvals_inner(app, *b)
        }
        // ask_user_question timeout: if the rollback equals the effective default, restore to None (keeps the mirror in sync with disk)
        ("toolset.ask_user_question.timeout_enabled", SettingValue::Bool(b)) => {
            if Some(*b) == pr13_effective_default("toolset.ask_user_question.timeout_enabled") {
                app.ask_user_question_timeout_enabled = None;
            } else {
                set_ask_user_question_timeout_enabled_inner(app, *b);
            }
        }
        ("show_thinking_blocks", SettingValue::Bool(b)) => set_show_thinking_blocks_inner(app, *b),
        ("group_tool_verbs", SettingValue::Bool(b)) => set_group_tool_verbs_inner(app, *b),
        ("collapsed_edit_blocks", SettingValue::Bool(b)) => {
            set_collapsed_edit_blocks_inner(app, *b)
        }
        ("prompt_suggestions", SettingValue::Bool(b)) => set_prompt_suggestions_inner(app, *b),
        // keep_text_selection: restore the cache mirror to the canonical value.
        ("keep_text_selection", SettingValue::Enum(s)) => {
            if let Some(kind) = crate::appearance::TextSelection::from_canonical(s) {
                set_keep_text_selection_inner(kind);
            }
        }
        // render_mermaid: restore the cache mirror to the canonical value.
        ("render_mermaid", SettingValue::Enum(s)) => {
            if let Some(kind) = crate::appearance::RenderMermaid::from_canonical(s) {
                set_render_mermaid_inner(kind);
            }
        }
        // hunk_tracker_mode: restore the in-memory ui mirror.
        ("hunk_tracker_mode", SettingValue::Enum(s)) => {
            set_hunk_tracker_mode_inner(app, crate::settings::canonical_hunk_tracker_mode(Some(s)));
        }
        ("screen_mode", SettingValue::Enum(s)) => {
            set_screen_mode_inner(app, crate::settings::canonical_screen_mode(Some(s)));
        }
        ("voice_keybind_enabled", SettingValue::Bool(b)) => {
            set_voice_keybind_enabled_inner(app, *b)
        }
        ("voice_capture_mode", SettingValue::Enum(s)) => {
            set_voice_capture_mode_inner(
                app,
                crate::settings::canonical_voice_capture_mode(Some(s)),
            );
        }
        ("voice_stt_language", SettingValue::Enum(s)) => {
            set_voice_stt_language_inner(
                app,
                crate::settings::canonical_voice_stt_language(Some(s)),
            );
        }
        // show_tips / auto_update: if the rollback equals the effective default, restore to None (keeps the mirror in sync with disk)
        ("show_tips", SettingValue::Bool(b)) => {
            if Some(*b) == pr13_effective_default("show_tips") {
                app.show_tips = None;
            } else {
                set_show_tips_inner(app, *b);
            }
        }
        ("auto_update", SettingValue::Bool(b)) => {
            if Some(*b) == pr13_effective_default("auto_update") {
                app.auto_update = None;
            } else {
                set_auto_update_inner(app, *b);
            }
        }
        // fork_secondary_model: empty rollback restores baseline default.
        ("fork_secondary_model", SettingValue::String(s)) => {
            let restored = if s.is_empty() {
                distill_shell::models::default_model().to_string()
            } else {
                s.clone()
            };
            set_fork_secondary_model_inner(app, restored);
        }
        // cheap_model: no pager-side mirror to roll back. It lives in the
        // config file the jev lanes resolve at startup, so the failure toast is
        // the whole story.
        ("cheap_model", SettingValue::String(_)) => {}

        _ => {
            tracing::error!(
                target: "settings",
                ?key,
                ?rollback_value,
                "rollback path has no arm for this setting key; in-memory cache is now \
                 inconsistent with the on-disk state (which already failed to write)"
            );
            app.show_toast(ROLLBACK_NO_ARM_TOAST);
            return companion_effects;
        }
    }
    refresh_open_settings_modals(app);
    companion_effects
}
