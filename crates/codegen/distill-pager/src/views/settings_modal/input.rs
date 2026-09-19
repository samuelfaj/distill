use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::layout::Rect;

use super::render::int_step_sizes;
use super::state::{
    RowEntry, SettingsKeyOutcome, SettingsModalState, SettingsMode, SettingsModeKind,
    action_for_bool, action_for_enum, action_for_enum_commit, action_for_int, action_for_string,
    effective_enum_choices, group_children, validate_string,
};
use crate::app::actions::Action;
use crate::input::line_editor::LineEditOutcome;
use crate::settings::{
    SettingKey, SettingKind, SettingValue, StringValidator, dynamic_enum_choices,
};

/// F2/Ctrl+,/Cmd+, always close regardless of mode. Space/Enter Repeat events are suppressed to
/// avoid per-tick disk writes.
pub fn handle_settings_key(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    if key.kind == KeyEventKind::Release {
        return SettingsKeyOutcome::Unchanged;
    }

    if key.kind == KeyEventKind::Repeat && matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
        return SettingsKeyOutcome::Unchanged;
    }

    if is_close_key(key) {
        return SettingsKeyOutcome::Close;
    }

    match state.state.mode_kind() {
        SettingsModeKind::Browse => handle_browse(state, key),
        SettingsModeKind::FilterFocused => handle_filter_focused(state, key),
        SettingsModeKind::PickingEnum => handle_picking_enum(state, key),
        SettingsModeKind::PickingGroup => handle_picking_group(state, key),
        SettingsModeKind::EditingString | SettingsModeKind::EditingInt => {
            handle_editing_value(state, key)
        }
    }
}

pub fn handle_settings_paste(state: &mut SettingsModalState, text: &str) -> SettingsKeyOutcome {
    match state.state.mode_kind() {
        SettingsModeKind::FilterFocused => {
            let outcome = state.state.filter.insert_paste(text);
            apply_filter_edit(state, outcome)
        }
        SettingsModeKind::EditingString => {
            let (validator, outcome) = {
                let SettingsMode::EditingString {
                    editor, validator, ..
                } = &mut state.state.mode
                else {
                    unreachable!("mode kind changed before paste")
                };
                (
                    *validator,
                    editor.insert_paste_with_policy(text, safe_settings_char, usize::MAX),
                )
            };
            apply_string_edit(state, validator, outcome)
        }
        SettingsModeKind::Browse
        | SettingsModeKind::PickingEnum
        | SettingsModeKind::PickingGroup
        | SettingsModeKind::EditingInt => SettingsKeyOutcome::Unchanged,
    }
}

/// Key routing for the enum chooser: Up/Down dispatches preview actions, Enter commits the current choice, Esc reverts to the original value.
fn handle_picking_enum(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    let (setting_key, choices_idx, original_value, supports_preview) = match &state.state.mode {
        SettingsMode::PickingEnum {
            key,
            choices_idx,
            original_value,
            supports_preview,
        } => (
            *key,
            *choices_idx,
            original_value.clone(),
            *supports_preview,
        ),
        _ => unreachable!("picker handler requires PickingEnum state"),
    };

    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            let len = picker_choices_len(state, setting_key);
            if choices_idx + 1 >= len {
                return SettingsKeyOutcome::Unchanged;
            }
            set_picker_idx(
                state,
                setting_key,
                choices_idx + 1,
                original_value,
                supports_preview,
            )
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if choices_idx == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            set_picker_idx(
                state,
                setting_key,
                choices_idx - 1,
                original_value,
                supports_preview,
            )
        }
        KeyCode::Enter => {
            // Commit the focused choice; this is the only point in a picker's open-to-close cycle that fires
            // `Effect::PersistSetting`.
            let close = std::mem::take(&mut state.close_on_picker_exit);
            if !close {
                state.transition_to_browse();
            }
            let kind_is_dynamic = matches!(
                state.registry.find(setting_key).map(|m| &m.kind),
                Some(SettingKind::DynamicEnum { .. })
            );
            let commit = if kind_is_dynamic {
                picker_choice_at_owned(state, setting_key, choices_idx).and_then(|canonical| {
                    action_for_string(setting_key, canonical, &state.pager_snapshot)
                })
            } else {
                picker_choice_at(state, setting_key, choices_idx)
                    .and_then(|c| action_for_enum_commit(setting_key, c))
            };
            match (close, commit) {
                (true, Some(action)) => SettingsKeyOutcome::ActionThenClose(action),
                (true, None) => SettingsKeyOutcome::Close,
                (false, Some(action)) => SettingsKeyOutcome::Action(action),
                (false, None) => SettingsKeyOutcome::Changed,
            }
        }
        KeyCode::Esc => {
            let close = std::mem::take(&mut state.close_on_picker_exit);
            if !close {
                state.transition_to_browse();
            }
            if let SettingValue::Enum(orig) = &original_value
                && let Some(action) = action_for_enum(setting_key, orig)
            {
                return if close {
                    SettingsKeyOutcome::ActionThenClose(action)
                } else {
                    SettingsKeyOutcome::Action(action)
                };
            }
            if close {
                SettingsKeyOutcome::Close
            } else {
                SettingsKeyOutcome::Changed
            }
        }
        // `d` reset: close the picker, revert the preview if applicable, then open the reset-confirm overlay
        // Consent choosers opt out entirely (no footer hint, no hidden shortcut); reset stays reachable from the browse row
        KeyCode::Char('d')
            if key.modifiers.is_empty() && !crate::settings::is_consent_chooser(setting_key) =>
        {
            state.transition_to_browse();
            if supports_preview
                && let SettingValue::Enum(orig) = &original_value
                && let Some(revert) = action_for_enum(setting_key, orig)
            {
                return SettingsKeyOutcome::ActionPair(
                    revert,
                    Action::OpenResetConfirm { key: setting_key },
                );
            }
            SettingsKeyOutcome::Action(Action::OpenResetConfirm { key: setting_key })
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Key routing for the group sub-sheet.
/// Up/Down moves between the child toggles; Space/Enter toggles the focused child in place (the sheet stays open); Esc returns to Browse.
fn handle_picking_group(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    let (group_key, child_idx) = match &state.state.mode {
        SettingsMode::PickingGroup { key, child_idx } => (*key, *child_idx),
        _ => unreachable!("group handler requires PickingGroup state"),
    };
    let children = group_children(state, group_key);
    if children.is_empty() {
        // Defensive: a group with no children can't be navigated, so back out
        state.transition_to_browse();
        return SettingsKeyOutcome::Changed;
    }

    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            if child_idx + 1 >= children.len() {
                return SettingsKeyOutcome::Unchanged;
            }
            state.transition_to_picking_group(group_key, child_idx + 1);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if child_idx == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            state.transition_to_picking_group(group_key, child_idx - 1);
            SettingsKeyOutcome::Changed
        }
        // Space/Enter toggle the focused child Bool and stay in the sheet so the user can flip several tips in a row
        // The dispatcher refreshes the modal snapshot, so the new value paints on the next frame
        KeyCode::Char(' ') | KeyCode::Enter => {
            let Some(child_key) = children.get(child_idx).copied() else {
                return SettingsKeyOutcome::Unchanged;
            };
            let cur = match state.value_for(child_key) {
                Some(SettingValue::Bool(b)) => b,
                _ => return SettingsKeyOutcome::Unchanged,
            };
            match action_for_bool(child_key, !cur) {
                Some(action) => SettingsKeyOutcome::Action(action),
                None => SettingsKeyOutcome::Unchanged,
            }
        }
        KeyCode::Esc => {
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Shared nav body for the picker's Up/Down (and j/k aliases).
/// Updates `choices_idx` in place, looks up the new canonical, and fires the preview dispatch via `action_for_enum`.
/// `new_idx` must be in bounds. Preview only fires for Enums with `supports_preview: true`; side-effecting Enums skip it.
pub(super) fn set_picker_idx(
    state: &mut SettingsModalState,
    setting_key: SettingKey,
    new_idx: usize,
    original_value: SettingValue,
    supports_preview: bool,
) -> SettingsKeyOutcome {
    let in_bounds = new_idx < picker_choices_len(state, setting_key);
    if !in_bounds {
        // The caller bounds-checks already; this re-check protects a future caller that does not
        return SettingsKeyOutcome::Unchanged;
    }
    // Focus moves cancel a pending double-click.
    state.picker_last_click = None;
    state.transition_to_picking_enum(setting_key, new_idx, original_value, supports_preview);
    // Preview dispatch for static Enums with preview support.
    if supports_preview
        && let Some(new_canonical) = picker_choice_at(state, setting_key, new_idx)
        && let Some(action) = action_for_enum(setting_key, new_canonical)
    {
        return SettingsKeyOutcome::Action(action);
    }
    SettingsKeyOutcome::Changed
}

/// Key routing for the inline string/int editor. Esc cancels, Enter commits.
/// String mode is free-form text with a cursor.
/// Int mode is a range-aware stepper (Up/Down small, Left/Right large; see [`int_step_sizes`]), clamped to [min,max].
fn handle_editing_value(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    // Int settings dispatch through a stepper-only handler
    // Char input, cursor panning, Backspace, Delete, Home, and End are rejected; only Up/Down/Left/Right (and j/k/h/l), Enter, and Esc do anything
    if let SettingsMode::EditingInt {
        key: setting_key,
        buffer,
        min,
        max,
    } = &state.state.mode
    {
        let setting_key = *setting_key;
        let buffer = buffer.clone();
        return handle_int_stepper(state, key, setting_key, &buffer, *min, *max);
    }

    let (setting_key, validator) = match &state.state.mode {
        SettingsMode::EditingString { key, validator, .. } => (*key, *validator),
        _ => unreachable!("editing handler requires String or Int state"),
    };

    if key.code == KeyCode::Enter {
        let SettingsMode::EditingString { editor, .. } = &state.state.mode else {
            unreachable!("String editor state changed during commit");
        };
        let text = editor.text().to_owned();
        let error = validate_string(validator, &text, &state.pager_snapshot.available_models);
        if error.is_some() {
            let SettingsMode::EditingString {
                validation_error, ..
            } = &mut state.state.mode
            else {
                unreachable!("String editor state changed during validation");
            };
            *validation_error = error;
            return SettingsKeyOutcome::Unchanged;
        }
        let action = action_for_string(setting_key, text, &state.pager_snapshot);
        state.transition_to_browse();
        return match action {
            Some(action) => SettingsKeyOutcome::Action(action),
            None => {
                tracing::error!(
                    target: "settings",
                    key = setting_key,
                    "EditingValue commit has no action_for_string arm — registry skew",
                );
                SettingsKeyOutcome::Changed
            }
        };
    }

    if key.code == KeyCode::Esc {
        state.transition_to_browse();
        return SettingsKeyOutcome::Changed;
    }

    if matches!(
        key.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Tab
            | KeyCode::BackTab
    ) {
        return SettingsKeyOutcome::Unchanged;
    }

    let outcome = {
        let SettingsMode::EditingString { editor, .. } = &mut state.state.mode else {
            unreachable!("String editor state changed before key handling");
        };
        editor.handle_key_with_insert_policy(key, safe_settings_char)
    };
    apply_string_edit(state, validator, outcome)
}

fn apply_string_edit(
    state: &mut SettingsModalState,
    validator: StringValidator,
    outcome: LineEditOutcome,
) -> SettingsKeyOutcome {
    match outcome {
        LineEditOutcome::TextChanged => {
            let SettingsMode::EditingString { editor, .. } = &state.state.mode else {
                unreachable!("String editor state changed after text mutation");
            };
            let error = validate_string(
                validator,
                editor.text(),
                &state.pager_snapshot.available_models,
            );
            let SettingsMode::EditingString {
                validation_error, ..
            } = &mut state.state.mode
            else {
                unreachable!("String editor state changed during validation");
            };
            *validation_error = error;
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::HandledNoChange | LineEditOutcome::CursorChanged => {
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::Unhandled => SettingsKeyOutcome::Unchanged,
    }
}

/// Steps by range-aware small (Up/Down) or large (Left/Right) deltas from [`int_step_sizes`], clamped to [min,max].
/// Non-stepper keys are rejected.
fn handle_int_stepper(
    state: &mut SettingsModalState,
    key: &KeyEvent,
    setting_key: SettingKey,
    buffer: &str,
    min: i64,
    max: i64,
) -> SettingsKeyOutcome {
    let (small_step, large_step) = int_step_sizes(min, max);
    let step_delta = |dir: i64, large: bool| -> i64 {
        let magnitude = if large { large_step } else { small_step };
        dir * magnitude
    };

    let apply_step = |state: &mut SettingsModalState, delta: i64| -> SettingsKeyOutcome {
        let cur = buffer.parse::<i64>().unwrap_or(min);
        let new = cur.saturating_add(delta).clamp(min, max);
        if new == cur {
            // Already clamped, so no visible change
            // Report Unchanged so the `clamps_to_min/max` tests can distinguish a no-op from a step
            return SettingsKeyOutcome::Unchanged;
        }
        let new_buf = new.to_string();
        update_int_buffer(state, new_buf);
        SettingsKeyOutcome::Changed
    };

    // Only modifier-free or SHIFT+arrow events trigger the stepper
    // Ctrl/Alt/etc belong to other editor features (selection extend, history) that the stepper has no notion of
    if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
        return SettingsKeyOutcome::Unchanged;
    }

    match key.code {
        KeyCode::Esc => {
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Enter => {
            // Commit. The buffer is guaranteed in range by the clamp on every step; parse it and dispatch.
            let action_opt = buffer
                .parse::<i64>()
                .ok()
                .and_then(|i| action_for_int(setting_key, i));
            state.transition_to_browse();
            match action_opt {
                Some(action) => SettingsKeyOutcome::Action(action),
                None => {
                    tracing::error!(
                        target: "settings",
                        key = setting_key,
                        "Int stepper Enter has no action_for_int arm — registry skew",
                    );
                    SettingsKeyOutcome::Changed
                }
            }
        }
        // Up / k: small step up.
        KeyCode::Up | KeyCode::Char('k') => apply_step(state, step_delta(1, false)),
        // Down / j: small step down.
        KeyCode::Down | KeyCode::Char('j') => apply_step(state, step_delta(-1, false)),
        // Right / l: large step up.
        KeyCode::Right | KeyCode::Char('l') => apply_step(state, step_delta(1, true)),
        // Left / h: large step down.
        KeyCode::Left | KeyCode::Char('h') => apply_step(state, step_delta(-1, true)),
        // `d` in the Int stepper dispatches `OpenResetConfirm` like Browse mode does
        // Close the stepper first so dispatch finds `ActiveModal::Settings` (the dispatch arm panics in debug mode on a non-Settings modal)
        // The stepper otherwise rejects letters, so intercepting `d` collides with nothing
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            state.transition_to_browse();
            SettingsKeyOutcome::Action(Action::OpenResetConfirm { key: setting_key })
        }
        // Everything else (digits, letters, Backspace, Delete, Home, End, Tab) is silently ignored; the stepper is not a text input
        _ => SettingsKeyOutcome::Unchanged,
    }
}

fn update_int_buffer(state: &mut SettingsModalState, new_buffer: String) {
    let SettingsMode::EditingInt { buffer, .. } = &mut state.state.mode else {
        unreachable!("Int update requires EditingInt state");
    };
    *buffer = new_buffer;
}

/// Number of choices for the picker.
/// Handles both `SettingKind::Enum` (static catalog) and `SettingKind::DynamicEnum` (catalog built from the snapshot at picker-open time).
pub(super) fn picker_choices_len(state: &SettingsModalState, key: SettingKey) -> usize {
    state
        .registry
        .find(key)
        .and_then(|m| match &m.kind {
            SettingKind::Enum { choices, .. } => {
                Some(effective_enum_choices(key, choices, &state.pager_snapshot).len())
            }
            SettingKind::DynamicEnum { source, .. } => {
                Some(dynamic_enum_choices(*source, &state.pager_snapshot).len())
            }
            _ => None,
        })
        .unwrap_or(0)
}

/// Canonical value at index `idx` in the picker's choices, or `None` if the key isn't a registered
/// Enum/DynamicEnum or `idx` is out of bounds.
pub(super) fn picker_choice_at(
    state: &SettingsModalState,
    key: SettingKey,
    idx: usize,
) -> Option<&'static str> {
    let meta = state.registry.find(key)?;
    let SettingKind::Enum { choices, .. } = &meta.kind else {
        return None;
    };
    effective_enum_choices(key, choices, &state.pager_snapshot)
        .get(idx)
        .map(|c| c.canonical)
}

/// Allocates one `String` per call; the picker calls it on commit, and per Up/Down only when
/// `supports_preview` is true, so the cost is bounded.
fn picker_choice_at_owned(
    state: &SettingsModalState,
    key: SettingKey,
    idx: usize,
) -> Option<String> {
    let meta = state.registry.find(key)?;
    match &meta.kind {
        SettingKind::Enum { choices, .. } => {
            effective_enum_choices(key, choices, &state.pager_snapshot)
                .get(idx)
                .map(|c| c.canonical.to_string())
        }
        SettingKind::DynamicEnum { source, .. } => {
            let resolved = dynamic_enum_choices(*source, &state.pager_snapshot);
            resolved.get(idx).map(|c| c.canonical.clone())
        }
        _ => None,
    }
}

/// F2 / Ctrl+, / Cmd+, are the modal-internal close keys. `handle_filter_focused` has its own. EscEsc
/// arm that exits filter mode without closing.
fn is_close_key(key: &KeyEvent) -> bool {
    if key.code == KeyCode::F(2) {
        return true;
    }
    if key.code == KeyCode::Char(',')
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
    {
        return true;
    }
    false
}

fn changed_if(b: bool) -> SettingsKeyOutcome {
    if b {
        SettingsKeyOutcome::Changed
    } else {
        SettingsKeyOutcome::Unchanged
    }
}

/// When a sub-pane mouse handler returns `Unchanged` but the breadcrumb hover flipped, upgrade to `Changed` so the renderer repaints the breadcrumb.
/// Non-`Unchanged` outcomes pass through so an `Action` or `Changed` from the inner handler keeps its meaning.
fn upgrade_if_breadcrumb_flipped(
    outcome: SettingsKeyOutcome,
    breadcrumb_flipped: bool,
) -> SettingsKeyOutcome {
    if breadcrumb_flipped && matches!(outcome, SettingsKeyOutcome::Unchanged) {
        SettingsKeyOutcome::Changed
    } else {
        outcome
    }
}

fn handle_browse(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => changed_if(state.advance_next()),
        KeyCode::Up | KeyCode::Char('k') => changed_if(state.advance_prev()),
        KeyCode::PageDown => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_next();
            }
            changed_if(moved)
        }
        KeyCode::PageUp => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_prev();
            }
            changed_if(moved)
        }
        KeyCode::Char('g') if key.modifiers.is_empty() => {
            // First selectable row in the filtered set
            // With no filter active, `filtered_cache` is `(0..rows.len())`, so this resolves to the first row
            let first = state
                .filtered_cache
                .iter()
                .copied()
                .find(|&idx| {
                    state
                        .rows
                        .get(idx)
                        .is_some_and(|r| matches!(r, RowEntry::Setting { .. }))
                })
                .unwrap_or(state.selected);
            if first != state.selected {
                state.selected = first;
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        KeyCode::Char('G') => {
            // Last selectable row in the filtered set
            let last = state
                .filtered_cache
                .iter()
                .rev()
                .copied()
                .find(|&idx| {
                    state
                        .rows
                        .get(idx)
                        .is_some_and(|r| matches!(r, RowEntry::Setting { .. }))
                })
                .unwrap_or(state.selected);
            if last != state.selected {
                state.selected = last;
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        // Right/`l` expands the focused row's description inline; Left/`h` collapses it
        // Expansion is per-row and persists across selection moves; multiple rows can be expanded at once
        KeyCode::Right | KeyCode::Char('l') if key.modifiers.is_empty() => {
            if let Some((key, _meta)) = state.focused_setting()
                && state.expanded_keys.insert(key)
            {
                return SettingsKeyOutcome::Changed;
            }
            SettingsKeyOutcome::Unchanged
        }
        KeyCode::Left | KeyCode::Char('h') if key.modifiers.is_empty() => {
            if let Some((key, _meta)) = state.focused_setting()
                && state.expanded_keys.remove(key)
            {
                return SettingsKeyOutcome::Changed;
            }
            SettingsKeyOutcome::Unchanged
        }
        KeyCode::Char(' ') => {
            if let Some(action) = state.toggle_focused_bool() {
                SettingsKeyOutcome::Action(action)
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        KeyCode::Enter => {
            // A group row opens its sub-sheet of child toggles
            if state.try_enter_picking_group() {
                return SettingsKeyOutcome::Changed;
            }
            // For Bool, Enter behaves like Space: both keys toggle
            if let Some(action) = state.toggle_focused_bool() {
                return SettingsKeyOutcome::Action(action);
            }
            // An Enum row enters PickingEnum mode; the picker sub-pane takes over rendering and key routing from here
            if state.try_enter_picking_enum() {
                return SettingsKeyOutcome::Changed;
            }
            // A String or Int row enters EditingValue mode; the inline editor takes over rendering and key routing
            if state.try_enter_editing_value() {
                return SettingsKeyOutcome::Changed;
            }
            SettingsKeyOutcome::Unchanged
        }
        // `i` aliases `/` (vim-nav "press i to search").
        KeyCode::Char('/') | KeyCode::Char('i') if key.modifiers.is_empty() => {
            state.focus_filter();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            // Reset to default: resolve the focused row's setting key and dispatch `Action::OpenResetConfirm`.
            // Cancel therefore returns to this exact modal state, with filter, scroll, and selection
            // preserved. Headers and unmapped rows are no-ops; `d` only acts on a focused setting row.
            match state.focused_setting() {
                // Group rows have no scalar default to reset.
                Some((_, meta)) if matches!(meta.kind, SettingKind::Group { .. }) => {
                    SettingsKeyOutcome::Unchanged
                }
                // A locked row isn't the user's to change, by `d` any more than by Enter (which `try_enter_picking_enum` refuses)
                // The dispatch-time guard would catch it anyway, but only after a confirm dialog for a change that cannot happen
                Some((key, _meta)) if state.row_lock(key).is_some() => {
                    SettingsKeyOutcome::Unchanged
                }
                Some((key, _meta)) => SettingsKeyOutcome::Action(Action::OpenResetConfirm { key }),
                // The focused row is a header (or out of bounds), so `d` has nothing to reset
                None => SettingsKeyOutcome::Unchanged,
            }
        }
        KeyCode::Backspace => {
            // Continue editing a committed query without refocusing the filter.
            if state.query().is_empty() {
                return SettingsKeyOutcome::Unchanged;
            }
            let outcome = state.state.filter.delete_last_grapheme();
            apply_filter_edit(state, outcome)
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

fn handle_filter_focused(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    match key.code {
        KeyCode::Esc => {
            if !state.query().is_empty() {
                state.state.filter.reset();
                state.invalidate_filter();
                state.clamp_selected_to_visible();
            }
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Enter => {
            // Commit the filter: exit FilterFocused and return to Browse, preserving the query
            // The user can then Space/Enter the focused filtered setting at once; clearing here would force re-navigating the full list
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Down => changed_if(state.advance_next()),
        KeyCode::Up => changed_if(state.advance_prev()),
        KeyCode::PageDown => {
            // Match Browse mode's PageDown: advance 10 rows
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_next();
            }
            changed_if(moved)
        }
        KeyCode::PageUp => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_prev();
            }
            changed_if(moved)
        }
        KeyCode::Tab => SettingsKeyOutcome::Unchanged,
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            if !state.query().is_empty() {
                state.state.filter.reset();
                state.invalidate_filter();
                state.clamp_selected_to_visible();
            }
            SettingsKeyOutcome::Changed
        }
        _ => {
            let outcome = state
                .state
                .filter
                .handle_key_with_insert_policy(key, safe_settings_char);
            apply_filter_edit(state, outcome)
        }
    }
}

fn safe_settings_char(character: char) -> bool {
    !crate::render::line_utils::is_unsafe_display_char(character)
}

#[cfg(test)]
pub(super) fn set_filter_cursor(state: &mut SettingsModalState, cursor_byte: usize) {
    let _ = state.state.filter.set_cursor_byte(cursor_byte);
}

fn apply_filter_edit(
    state: &mut SettingsModalState,
    outcome: LineEditOutcome,
) -> SettingsKeyOutcome {
    match outcome {
        LineEditOutcome::TextChanged => {
            state.invalidate_filter();
            state.clamp_selected_to_visible();
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::HandledNoChange | LineEditOutcome::CursorChanged => {
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::Unhandled => SettingsKeyOutcome::Unchanged,
    }
}

/// Handle a mouse event in the modal content area.
pub fn handle_settings_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    // The breadcrumb is hierarchical "up": always return to Browse, never dismiss via `close_on_picker_exit`
    // Clearing the deep-link flag first lets the sub-pane Esc handlers do the preview revert
    if matches!(
        kind,
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
    ) && let Some(rect) = state.settings_breadcrumb_rect
        && rect_contains(rect, column, row)
    {
        state.close_on_picker_exit = false;
        let synthetic = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        match state.state.mode_kind() {
            SettingsModeKind::PickingEnum => {
                return handle_picking_enum(state, &synthetic);
            }
            SettingsModeKind::PickingGroup => {
                return handle_picking_group(state, &synthetic);
            }
            SettingsModeKind::EditingString | SettingsModeKind::EditingInt => {
                return handle_editing_value(state, &synthetic);
            }
            _ => {}
        }
    }

    // Track hover for the breadcrumb hit-rect so the renderer can repaint the title with the brighter `accent_user` fg when the mouse is over it
    // Without this cue the breadcrumb looks no different from the rest of the modal title
    // Tracked here so a hover transition registers even when the row-list, picker, or editor mouse handlers below short-circuit on the Moved event
    let breadcrumb_hover_flipped = if matches!(kind, MouseEventKind::Moved) {
        let now_hovered = state
            .settings_breadcrumb_rect
            .map(|r| rect_contains(r, column, row))
            .unwrap_or(false);
        let flipped = now_hovered != state.breadcrumb_hovered;
        state.breadcrumb_hovered = now_hovered;
        // In sub-pane modes the mouse handlers don't update hover_row, so a flipped breadcrumb hover is the only thing that could redraw
        // In Browse the row-list handler below already returns Changed when hover_row moves, so it runs as usual
        flipped
    } else {
        false
    };

    // Handle `[-]` / `[+]` clicks when in EditingValue mode and the row is an Int
    // All other events in EditingValue (scrolls, off-adornment clicks) are no-ops
    if matches!(
        state.state.mode_kind(),
        SettingsModeKind::EditingString | SettingsModeKind::EditingInt
    ) {
        let outcome = handle_editor_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    // PickingEnum: click-to-pick on choice rects; the scroll wheel is a no-op (the picker is bounded; scrolling there could surprise)
    if state.state.mode_kind() == SettingsModeKind::PickingEnum {
        let outcome = handle_picker_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    // PickingGroup: hover tracks the child rects; a click toggles the clicked child in place
    // As in the enum picker, the viewport is bounded and scroll is a no-op
    if state.state.mode_kind() == SettingsModeKind::PickingGroup {
        let outcome = handle_group_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    let on_list = rect_contains(state.list_area, column, row);

    // Mouse hover highlight (parity with scrollback).
    // Walk `state.row_rects` to find the row under the cursor and update `state.hover_row`
    // Return early so later arms don't repeat the find; clicks and scrolls fall through to the arms below
    if matches!(kind, MouseEventKind::Moved) {
        let new_hover = state
            .row_rects
            .iter()
            .position(|r| rect_contains(*r, column, row))
            .filter(|&idx| matches!(state.rows.get(idx), Some(RowEntry::Setting { .. })));
        if new_hover != state.hover_row {
            state.hover_row = new_hover;
            return SettingsKeyOutcome::Changed;
        }
        return SettingsKeyOutcome::Unchanged;
    }

    match kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if !on_list {
                return SettingsKeyOutcome::Unchanged;
            }
            // Resolve the clicked row.
            let clicked_idx = state
                .row_rects
                .iter()
                .position(|r| rect_contains(*r, column, row));
            let Some(idx) = clicked_idx else {
                return SettingsKeyOutcome::Unchanged;
            };
            // Clicking a header is a no-op.
            if !state
                .rows
                .get(idx)
                .is_some_and(|r| matches!(r, RowEntry::Setting { .. }))
            {
                return SettingsKeyOutcome::Unchanged;
            }
            // Two-stage clicks. Click on a different row: only select, so the user can read the description
            // first.
            let Some(&row_rect) = state.row_rects.get(idx) else {
                return SettingsKeyOutcome::Unchanged;
            };
            // Col 0 of the row is the `▸`/`▾` triangle glyph. A click there toggles expansion without touching
            // the value, matching the keyboard's Right/Left arrows. Two-line rows have `row_rect.height = 2`
            // with the triangle on line 1 only.
            let on_triangle = column == row_rect.x && row == row_rect.y;
            // The 5-col indicator hit-rect sits on the value column on the right, not the left edge.
            let value_rect = state.value_hit_rects.get(idx).copied().unwrap_or_default();
            let on_value = rect_contains(value_rect, column, row);
            let was_selected_already = state.selected == idx;
            let _ = state.select_at(idx);

            if on_triangle && let Some((key, _meta)) = state.focused_setting() {
                // Toggle expansion, mirroring the keyboard Right/Left arrows
                if state.expanded_keys.contains(key) {
                    state.expanded_keys.remove(key);
                } else {
                    state.expanded_keys.insert(key);
                }
                return SettingsKeyOutcome::Changed;
            }

            if on_value || was_selected_already {
                if state.try_enter_picking_group() {
                    return SettingsKeyOutcome::Changed;
                }
                if let Some(action) = state.toggle_focused_bool() {
                    return SettingsKeyOutcome::Action(action);
                }
                if state.try_enter_picking_enum() || state.try_enter_editing_value() {
                    return SettingsKeyOutcome::Changed;
                }
            }
            // Selection moved (or was already on this row); the re-render reflects the new focus
            SettingsKeyOutcome::Changed
        }
        MouseEventKind::ScrollDown => {
            if !on_list {
                return SettingsKeyOutcome::Unchanged;
            }
            let mut moved = false;
            for _ in 0..3 {
                moved |= state.advance_next();
            }
            changed_if(moved)
        }
        MouseEventKind::ScrollUp => {
            if !on_list {
                return SettingsKeyOutcome::Unchanged;
            }
            let mut moved = false;
            for _ in 0..3 {
                moved |= state.advance_prev();
            }
            changed_if(moved)
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Handle a mouse event while the modal is in `PickingEnum` mode.
fn handle_picker_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    // Hover highlight for picker choices
    // Track the choice index under the cursor in `state.hover_row` (the same field the row-list path uses; it is mode-aware)
    if matches!(kind, MouseEventKind::Moved) {
        let new_hover = state
            .picker_choice_rects
            .iter()
            .position(|r| r.height > 0 && rect_contains(*r, column, row));
        if new_hover != state.hover_row {
            state.hover_row = new_hover;
            return SettingsKeyOutcome::Changed;
        }
        return SettingsKeyOutcome::Unchanged;
    }

    let MouseEventKind::Down(crossterm::event::MouseButton::Left) = kind else {
        return SettingsKeyOutcome::Unchanged;
    };
    // Snapshot the picker payload before mutating the state.
    let (setting_key, current_idx, original_value, supports_preview) = match &state.state.mode {
        SettingsMode::PickingEnum {
            key,
            choices_idx,
            original_value,
            supports_preview,
        } => (
            *key,
            *choices_idx,
            original_value.clone(),
            *supports_preview,
        ),
        _ => unreachable!("picker mouse handler requires PickingEnum state"),
    };
    let clicked_idx = state
        .picker_choice_rects
        .iter()
        .position(|r| r.height > 0 && rect_contains(*r, column, row));
    let Some(target_idx) = clicked_idx else {
        state.picker_last_click = None;
        return SettingsKeyOutcome::Unchanged;
    };
    // Do not move focus and then Enter-commit: that drops the preview Action
    // and can persist a different radio than the one clicked.
    if picker_click_is_double(state, target_idx) && target_idx == current_idx {
        state.picker_last_click = None;
        let synthetic = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        return handle_picking_enum(state, &synthetic);
    }
    let outcome = if target_idx == current_idx {
        SettingsKeyOutcome::Unchanged
    } else {
        set_picker_idx(
            state,
            setting_key,
            target_idx,
            original_value,
            supports_preview,
        )
    };
    // Arm after `set_picker_idx`, which clears a pending double-click.
    state.picker_last_click = Some((target_idx, std::time::Instant::now()));
    outcome
}

fn picker_click_is_double(state: &SettingsModalState, idx: usize) -> bool {
    state.picker_last_click.is_some_and(|(prev, at)| {
        prev == idx && at.elapsed().as_millis() < crate::app::agent_view::MULTI_CLICK_TIMEOUT_MS
    })
}

/// Handle a mouse event while the modal is in `PickingGroup` mode.
fn handle_group_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    if matches!(kind, MouseEventKind::Moved) {
        let new_hover = state
            .picker_choice_rects
            .iter()
            .position(|r| r.height > 0 && rect_contains(*r, column, row));
        if new_hover != state.hover_row {
            state.hover_row = new_hover;
            return SettingsKeyOutcome::Changed;
        }
        return SettingsKeyOutcome::Unchanged;
    }
    let MouseEventKind::Down(crossterm::event::MouseButton::Left) = kind else {
        return SettingsKeyOutcome::Unchanged;
    };
    let group_key = match &state.state.mode {
        SettingsMode::PickingGroup { key, .. } => *key,
        _ => unreachable!("group mouse handler requires PickingGroup state"),
    };
    let children = group_children(state, group_key);
    let clicked_idx = state
        .picker_choice_rects
        .iter()
        .position(|r| r.height > 0 && rect_contains(*r, column, row));
    let Some(idx) = clicked_idx else {
        return SettingsKeyOutcome::Unchanged;
    };
    state.transition_to_picking_group(group_key, idx);
    let Some(child_key) = children.get(idx).copied() else {
        return SettingsKeyOutcome::Changed;
    };
    let cur = matches!(state.value_for(child_key), Some(SettingValue::Bool(true)));
    match action_for_bool(child_key, !cur) {
        Some(action) => SettingsKeyOutcome::Action(action),
        None => SettingsKeyOutcome::Changed,
    }
}

/// Handle a mouse event while in `EditingValue` mode.
/// Clicks on the Int editor's `[-]` / `[+]` adornments dispatch as keyboard-equivalent Down/Up steps; everything else is a no-op.
fn handle_editor_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    let MouseEventKind::Down(crossterm::event::MouseButton::Left) = kind else {
        return SettingsKeyOutcome::Unchanged;
    };
    let (dec_rect, inc_rect) = state.editor_adornment_rects;
    let step_dir = if rect_contains(dec_rect, column, row) {
        StepDir::Down
    } else if rect_contains(inc_rect, column, row) {
        StepDir::Up
    } else {
        return SettingsKeyOutcome::Unchanged;
    };
    // Synthesize the equivalent keyboard event so the step, clamp, and validation logic lives in one place (`handle_editing_value`)
    // Mirrors the picker's choice-click approach
    let synthetic = KeyEvent::new(
        match step_dir {
            StepDir::Up => KeyCode::Up,
            StepDir::Down => KeyCode::Down,
        },
        KeyModifiers::NONE,
    );
    handle_editing_value(state, &synthetic)
}

/// Direction tag for turning the Int editor's spinner clicks into key events; internal to `handle_editor_mouse`.
#[derive(Clone, Copy)]
enum StepDir {
    Up,
    Down,
}

fn rect_contains(r: Rect, column: u16, row: u16) -> bool {
    r.width > 0
        && r.height > 0
        && column >= r.x
        && column < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}
