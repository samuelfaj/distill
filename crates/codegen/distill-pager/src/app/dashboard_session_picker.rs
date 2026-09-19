use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::app::actions::Action;
use crate::app::app_view::{AppView, InputOutcome};
use crate::views::modal_window::{ModalWindowOutcome, handle_modal_mouse};
use crate::views::picker::{PickerConfig, PickerOutcome, handle_picker_input};
use crate::views::session_picker::{
    PickerItem, build_entry_map, effective_filter_query, repo_name_from_cwd,
};
use crate::views::session_picker_surface::DASHBOARD_PICKER_TITLE;

impl AppView {
    pub(super) fn handle_dashboard_session_picker_input(
        &mut self,
        ev: &Event,
    ) -> Option<InputOutcome> {
        let picker_cwd = self
            .dashboard
            .as_ref()
            .map_or(self.cwd.as_path(), |dashboard| dashboard.cwd.as_path());
        let current_repo = repo_name_from_cwd(&picker_cwd.to_string_lossy());
        let surface = self.dashboard_session_picker.as_mut()?;
        if let Event::Mouse(mouse) = ev {
            match handle_modal_mouse(&mut surface.window, mouse.kind, mouse.column, mouse.row) {
                ModalWindowOutcome::CloseRequested => {
                    return Some(InputOutcome::Action(Action::DashboardCloseSessionPicker));
                }
                ModalWindowOutcome::Unhandled => {}
                _ => return Some(InputOutcome::Changed),
            }
        }
        if matches!(
            ev,
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && key.code == KeyCode::Char('f')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
        ) {
            return Some(InputOutcome::Unchanged);
        }
        let entry_map = build_entry_map(
            surface.entries.as_deref(),
            None,
            effective_filter_query(surface.state.query(), surface.entries_query.as_deref()),
            true,
            false,
            surface.source_filter,
            Some(current_repo.as_str()),
        );
        let non_selectable: Vec<bool> = entry_map.iter().map(Option::is_none).collect();
        let config = PickerConfig {
            title: Some(DASHBOARD_PICKER_TITLE),
            show_search_hint: true,
            expandable: false,
            esc_clears_query: true,
            shortcuts: Some(crate::views::picker::picker_shortcuts()),
            pending_hint: None,
            non_selectable: &non_selectable,
            non_selectable_clickable: &[],
            shortcuts_area: None,
            tabs: None,
            active_tab: 0,
            filter_label: None,
            filter_key_hint: None,
            filter_active: false,
            header_note: None,
            action_keys: &[],
            disable_search: false,
            compact_bottom_bar: false,
            search_only_on_slash: false,
            vim_normal_first: crate::appearance::cache::load_vim_mode(),
        };
        let outcome = handle_picker_input(ev, &mut surface.state, entry_map.len(), &config);
        Some(match outcome {
            PickerOutcome::Selected(index) => {
                match entry_map.get(index).and_then(|item| item.as_ref()) {
                    Some(PickerItem::Fuzzy { original_index }) => {
                        InputOutcome::Action(Action::DashboardPickSession(*original_index))
                    }
                    _ => InputOutcome::Changed,
                }
            }
            PickerOutcome::Closed => InputOutcome::Action(Action::DashboardCloseSessionPicker),
            PickerOutcome::Unchanged => InputOutcome::Unchanged,
            PickerOutcome::Changed | PickerOutcome::QueryChanged => InputOutcome::Changed,
            _ => InputOutcome::Changed,
        })
    }
}
