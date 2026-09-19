//! The Write step's editable enum labels: one full-width row per supplied enum above the
//! composer, plus the shared-picker popup that edits the focused row's value.
//!
//! The picker is the same list widget family as the command palette and arg picker. Enter on a
//! focused label row opens it; it owns key/paste/mouse input until Enter commits the highlighted
//! variant or Esc backs out, both landing back on the still-focused row.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use strum::IntoEnumIterator as _;

use super::{
    FeedbackFailureMode, FeedbackModalMetadata, FeedbackModalOutcome, FeedbackModalState,
    FeedbackTaskCategory, FeedbackType, MetadataField,
};
use crate::theme::Theme;
use crate::views::modal_window::Shortcut;
use crate::views::picker::{
    self, PickerConfig, PickerEntry, PickerOutcome, PickerRow, PickerState,
};

pub(super) struct EnumPicker {
    field: MetadataField,
    state: PickerState,
}

impl MetadataField {
    /// The field's fixed variant labels, in declaration (`EnumIter`) order; picker indices map
    /// back through this.
    pub(super) fn variant_labels(self) -> Vec<&'static str> {
        match self {
            Self::Type => FeedbackType::iter().map(|v| v.label()).collect(),
            Self::Task => FeedbackTaskCategory::iter().map(|v| v.label()).collect(),
            Self::Failure => FeedbackFailureMode::iter().map(|v| v.label()).collect(),
        }
    }

    /// Declaration-order indices whose labels contain `query` (case-insensitive); no variant is invented.
    fn filtered_variants(self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.variant_labels()
            .iter()
            .enumerate()
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }
}

impl FeedbackModalMetadata {
    /// Position of the field's current value in its declaration (`EnumIter`) order; the picker
    /// highlights it on open.
    pub(super) fn field_variant_index(&self, field: MetadataField) -> usize {
        match field {
            MetadataField::Type => self
                .r#type
                .as_ref()
                .and_then(|value| FeedbackType::iter().position(|v| v == *value)),
            MetadataField::Task => self
                .task_category
                .as_ref()
                .and_then(|value| FeedbackTaskCategory::iter().position(|v| v == *value)),
            MetadataField::Failure => self
                .failure_mode
                .as_ref()
                .and_then(|value| FeedbackFailureMode::iter().position(|v| v == *value)),
        }
        .unwrap_or(0)
    }

    /// Commit the declaration-order variant a picker selected. The stored value is what a later send carries.
    pub(super) fn set_field_variant(&mut self, field: MetadataField, index: usize) {
        match field {
            MetadataField::Type => {
                if let Some(value) = FeedbackType::iter().nth(index) {
                    self.r#type = Some(value);
                }
            }
            MetadataField::Task => {
                if let Some(value) = FeedbackTaskCategory::iter().nth(index) {
                    self.task_category = Some(value);
                }
            }
            MetadataField::Failure => {
                if let Some(value) = FeedbackFailureMode::iter().nth(index) {
                    self.failure_mode = Some(value);
                }
            }
        }
    }
}

impl FeedbackModalState {
    // Esc closes the picker back to the label rows, never the modal, so no cancel hint here.
    pub(super) const PICKER_SHORTCUTS: &[Shortcut<'static>] = &[
        Shortcut {
            label: "↑↓ move",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "type filter",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Enter select",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Esc back",
            clickable: false,
            id: 0,
        },
    ];

    /// One full-width row per supplied enum, stacked above the composer, in `fields`. (`field_order`)
    /// order. The focused row carries the selection marker in the emphasized style; the rest stay
    /// readable `text_secondary`, never `theme.gray`: the enums are editable, not disabled decoration.
    pub(super) fn render_metadata_rows(
        &mut self,
        buf: &mut Buffer,
        content: Rect,
        inner_x: u16,
        theme: &Theme,
        fields: &[MetadataField],
    ) {
        // Marker column: two cells left of the text when the gutter allows (normal h_pad = 2),
        // hugging the text in compact renders (h_pad = 1), never left of the modal's inner edge.
        let marker_x = content.x.saturating_sub(2).max(inner_x);
        for (index, field) in fields.iter().enumerate() {
            let y = content.y + index as u16;
            let Some(text) = self.metadata.field_text(*field) else {
                continue;
            };
            let style = if self.metadata_focus == Some(*field) {
                ratatui::style::Style::default()
                    .fg(theme.text_primary)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                ratatui::style::Style::default().fg(theme.text_secondary)
            };
            if self.metadata_focus == Some(*field) && marker_x < content.x {
                buf.set_string(marker_x, y, "\u{276F}", style);
            }
            buf.set_stringn(content.x, y, text, content.width as usize, style);
            // The clickable rect includes the marker gutter, so the whole visual row focuses.
            self.metadata_row_areas.push((
                *field,
                Rect::new(marker_x, y, content.width + (content.x - marker_x), 1),
            ));
        }
    }

    /// The label row under a screen position, from the last render's rects; the whole row is
    /// clickable, not just the text.
    pub(super) fn metadata_field_at(&self, column: u16, row: u16) -> Option<MetadataField> {
        self.metadata_row_areas
            .iter()
            .find(|(_, rect)| rect.contains(Position::new(column, row)))
            .map(|(field, _)| *field)
    }

    /// Open the picker over the focused row's fixed variants, highlighting the current value.
    pub(super) fn open_enum_picker(&mut self, field: MetadataField) {
        // Type-to-filter from the first keystroke, like the command palette and arg picker.
        let mut state = PickerState::input_active();
        state.selected = self.metadata.field_variant_index(field);
        self.enum_picker = Some(EnumPicker { field, state });
    }

    /// The picker's fixed input config: type-to-filter, Enter selects, Esc closes.
    fn enum_picker_config() -> PickerConfig<'static> {
        PickerConfig {
            title: None,
            show_search_hint: false,
            expandable: false,
            esc_clears_query: false,
            shortcuts: None,
            pending_hint: None,
            shortcuts_area: None,
            non_selectable: &[],
            non_selectable_clickable: &[],
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
            vim_normal_first: false,
        }
    }

    /// Route one event through the shared picker while it owns the keys.
    /// Enter commits the highlighted variant onto the metadata; Esc backs out without changing
    /// the value. Both land back on the still-focused label row, never on the composer.
    pub(super) fn handle_enum_picker_event(
        &mut self,
        ev: &crossterm::event::Event,
    ) -> FeedbackModalOutcome {
        let Some(picker) = self.enum_picker.as_mut() else {
            return FeedbackModalOutcome::Changed;
        };
        let field = picker.field;
        // Filtered declaration-order indices for the pre-event query: the selection the outcome names lives in this list.
        let filtered = field.filtered_variants(picker.state.query());
        let outcome = picker::handle_picker_input(
            ev,
            &mut picker.state,
            filtered.len(),
            &Self::enum_picker_config(),
        );
        match outcome {
            PickerOutcome::Selected(index) => {
                if let Some(&variant) = filtered.get(index) {
                    self.metadata.set_field_variant(field, variant);
                }
                self.enum_picker = None;
                self.metadata_focus = Some(field);
            }
            PickerOutcome::Closed => {
                self.enum_picker = None;
                self.metadata_focus = Some(field);
            }
            // Includes SubmitQuery: a filtered-to-empty query's Enter commits nothing.
            _ => {}
        }
        FeedbackModalOutcome::Changed
    }

    /// Render the picker in place of the composer (the label rows above stay as context).
    pub(super) fn render_enum_picker(
        &mut self,
        buf: &mut Buffer,
        content: Rect,
        inner_x: u16,
        inner_width: u16,
        theme: &Theme,
    ) {
        let Some(picker) = self.enum_picker.as_mut() else {
            return;
        };
        let labels = picker.field.variant_labels();
        let filtered = picker.field.filtered_variants(picker.state.query());
        let entries: Vec<PickerEntry<'_>> = filtered
            .iter()
            .enumerate()
            .map(|(vis, &variant)| {
                PickerEntry::Row(PickerRow {
                    label: labels.get(variant).copied().unwrap_or(""),
                    right_label: "",
                    selected: picker.state.hovered == Some(vis)
                        || (picker.state.hovered.is_none() && vis == picker.state.selected),
                    expanded: false,
                    fields: &[],
                    description_lines: &[],
                    summary_lines: &[],
                    dimmed: false,
                    indent: 0,
                    badge: "",
                    badge_color: None,
                    collapsible: false,
                    underline_last_desc: false,
                })
            })
            .collect();
        picker::render_picker_in_modal(
            buf,
            content,
            inner_x,
            inner_width,
            theme,
            &mut picker.state,
            &entries,
            &[],
            false,
        );
    }
}
