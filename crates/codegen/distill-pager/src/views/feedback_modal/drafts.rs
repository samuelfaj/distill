// Modified for Distill by Samuel Fajreldines, 2026.
//! Drafts tab: the stored-draft list, its request/completion handshake, and the unsaved-Write guard.

use crossterm::event::{KeyCode, KeyEvent};
use distill_feedback::{derive_title, post_text};

use super::{
    DRAFT_DOUBLE_CLICK_TIMEOUT, FeedbackCompositionId, FeedbackDraft, FeedbackDraftId,
    FeedbackFailureMode, FeedbackModalId, FeedbackModalMetadata, FeedbackModalOutcome,
    FeedbackModalState, FeedbackModalStep, FeedbackTab, FeedbackTaskCategory, FeedbackType,
};
use crate::input::line_editor::LineEditor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DraftSubmitTerminal {
    CleanupFailed,
    OutcomeUnknown,
}

#[derive(Debug)]
pub enum DraftsState {
    Unloaded,
    Loading {
        generation: u64,
    },
    Browse {
        rows: Vec<FeedbackDraft>,
        selected_id: Option<FeedbackDraftId>,
        query: LineEditor,
        search_focused: bool,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackDraftLoad {
    pub modal_id: FeedbackModalId,
    pub generation: u64,
    pub draft_id: FeedbackDraftId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackDraftDeleteToken(pub(super) u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackDraftDelete {
    pub modal_id: FeedbackModalId,
    pub token: FeedbackDraftDeleteToken,
    pub draft_id: FeedbackDraftId,
}

/// Type is required: the shell's `drafts/update` rejects a body without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackDraftUpdate {
    pub modal_id: FeedbackModalId,
    pub draft_id: FeedbackDraftId,
    pub details: String,
    pub title: String,
    pub area: Option<String>,
    pub r#type: FeedbackType,
    pub task_category: Option<FeedbackTaskCategory>,
    pub failure_mode: Option<FeedbackFailureMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackDraftRequest {
    List {
        modal_id: FeedbackModalId,
        generation: u64,
    },
    Load(FeedbackDraftLoad),
    Delete(FeedbackDraftDelete),
    Update(FeedbackDraftUpdate),
}

#[derive(Debug, Clone, Copy)]
enum DraftMove {
    Next,
    Previous,
    First,
    Last,
}

impl FeedbackModalState {
    pub fn take_pending_request(&mut self) -> Option<FeedbackDraftRequest> {
        self.pending_request.take()
    }

    pub fn start_external_draft_load(&mut self, draft_id: FeedbackDraftId) {
        self.metadata.draft_id = None;
        self.start_draft_load(draft_id);
    }

    pub fn apply_draft_list(
        &mut self,
        modal_id: FeedbackModalId,
        generation: u64,
        rows: Vec<FeedbackDraft>,
    ) {
        if !self.matches_id(modal_id)
            || !matches!(self.drafts, DraftsState::Loading { generation: pending } if pending == generation)
        {
            return;
        }
        let selected_id = rows.first().map(|draft| draft.id.clone());
        if self.open_on_drafts_if_any && rows.is_empty() {
            self.window.active_tab = FeedbackTab::Write.index();
        }
        self.open_on_drafts_if_any = false;
        self.drafts = DraftsState::Browse {
            rows,
            selected_id,
            query: LineEditor::default(),
            search_focused: false,
            error: None,
        };
    }

    pub fn fail_draft_list(&mut self, modal_id: FeedbackModalId, generation: u64, error: String) {
        if self.matches_id(modal_id)
            && matches!(self.drafts, DraftsState::Loading { generation: pending } if pending == generation)
        {
            // No list means nothing to browse; a peek-at-Drafts open lands on Write like an empty list.
            if self.open_on_drafts_if_any {
                self.window.active_tab = FeedbackTab::Write.index();
            }
            self.open_on_drafts_if_any = false;
            self.drafts = DraftsState::Browse {
                rows: Vec::new(),
                selected_id: None,
                query: LineEditor::default(),
                search_focused: false,
                error: Some(error),
            };
        }
    }

    pub fn apply_draft_load(&mut self, load: &FeedbackDraftLoad, draft: FeedbackDraft) -> bool {
        if self.draft_load.as_ref().is_none_or(|pending| {
            pending.modal_id != load.modal_id
                || pending.generation != load.generation
                || pending.draft_id != load.draft_id
        }) {
            return false;
        }
        self.draft_load = None;
        if self.submit_pending || self.is_draft_submit_terminal() {
            return false;
        }
        self.composition_id = FeedbackCompositionId(self.composition_id.0.wrapping_add(1));
        self.paste_probes_in_flight = 0;
        self.deferred_submit = false;
        self.window.active_tab = FeedbackTab::Write.index();
        self.composer.reconcile_feedback_images_on_teardown();
        self.composer.cleanup_images_on_teardown();
        self.composer.set_text("");
        self.composer.clear_history();
        self.composer.set_text(&draft.details);
        self.composer.clear_history();
        self.composer.set_cursor(draft.details.len());
        self.metadata = FeedbackModalMetadata {
            r#type: draft.r#type,
            task_category: draft.task_category,
            failure_mode: draft.failure_mode,
            draft_id: Some(draft.id),
        };
        self.field_order = self.metadata.draft_editor_fields();
        self.draft_title = Some(draft.title);
        self.draft_area = draft.area;
        self.capture_write_baseline();
        self.error = None;
        self.submit_terminal = None;
        self.step = FeedbackModalStep::Write;
        true
    }

    pub fn fail_draft_load(&mut self, load: &FeedbackDraftLoad, error: String) {
        if self.draft_load.as_ref().is_some_and(|pending| {
            pending.modal_id == load.modal_id
                && pending.generation == load.generation
                && pending.draft_id == load.draft_id
        }) {
            self.draft_load = None;
            self.window.active_tab = FeedbackTab::Drafts.index();
            if self.draft_delete.is_none() {
                self.refresh_drafts();
            }
            self.error = Some(error);
        }
    }

    pub fn apply_draft_delete(&mut self, delete: &FeedbackDraftDelete) {
        if self.draft_delete.as_ref() != Some(delete) {
            return;
        }
        self.draft_delete = None;
        self.delete_confirm = None;
        if self.metadata.draft_id.as_ref() == Some(&delete.draft_id) {
            self.metadata.draft_id = None;
            self.write_baseline.3.draft_id = None;
            self.draft_title = None;
            self.draft_area = None;
            self.write_baseline.1 = None;
            self.write_baseline.2 = None;
        }
        if let DraftsState::Browse {
            rows, selected_id, ..
        } = &mut self.drafts
        {
            let old_index = rows
                .iter()
                .position(|draft| draft.id == delete.draft_id)
                .unwrap_or(0);
            rows.retain(|draft| draft.id != delete.draft_id);
            *selected_id = rows
                .get(old_index.min(rows.len().saturating_sub(1)))
                .map(|draft| draft.id.clone());
        }
    }

    pub fn fail_draft_delete(&mut self, delete: &FeedbackDraftDelete, error: String) {
        if self.draft_delete.as_ref() == Some(delete) {
            self.draft_delete = None;
            self.delete_confirm = None;
            self.set_drafts_error(error);
        }
    }

    pub fn mark_draft_submit_pending(&mut self) {
        self.invalidate_draft_load();
        self.submit_pending = true;
        self.error = Some("Sending draft…".to_owned());
    }

    pub fn cancel_draft_submit_pending(&mut self, error: String) {
        self.submit_pending = false;
        self.error = Some(error);
    }

    pub fn mark_draft_cleanup_failed(&mut self) {
        self.invalidate_draft_load();
        self.submit_pending = false;
        self.submit_terminal = Some(DraftSubmitTerminal::CleanupFailed);
        self.step = FeedbackModalStep::Write;
        self.error = Some(
            "Feedback was sent, but the stored draft could not be deleted. Delete it manually; do not resend."
                .to_owned(),
        );
    }

    pub fn mark_draft_submit_unknown(&mut self) -> Option<String> {
        self.invalidate_draft_load();
        self.submit_pending = false;
        self.submit_terminal = Some(DraftSubmitTerminal::OutcomeUnknown);
        self.step = FeedbackModalStep::Write;
        let details = self.submitted_text();
        // Only reachable after a send, which `draft_body()` refused without a type.
        if let Some((draft_id, r#type)) = self.metadata.draft_id.clone().zip(self.metadata.r#type) {
            let title = self
                .draft_title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| derive_title(&details));
            self.pending_request = Some(FeedbackDraftRequest::Update(FeedbackDraftUpdate {
                modal_id: self.id,
                draft_id,
                details: details.clone(),
                title: title.clone(),
                area: self.draft_area.clone(),
                r#type,
                task_category: self.metadata.task_category,
                failure_mode: self.metadata.failure_mode,
            }));
            self.error = Some(
                "The remote outcome is unknown. The latest text was copied to the clipboard. Saving it back to this draft; close and do not resend."
                    .to_owned(),
            );
            let copy = post_text(&title, &details);
            return (!copy.trim().is_empty()).then_some(copy);
        }
        // A later POST success may already have deleted the row; never append a replacement.
        self.pending_request = None;
        self.error = Some(
            "The remote outcome is unknown. The latest text was copied to the clipboard. Close and do not resend."
                .to_owned(),
        );
        (!details.trim().is_empty()).then_some(details)
    }

    pub fn apply_draft_update_complete(
        &mut self,
        update: &FeedbackDraftUpdate,
        error: Option<&str>,
    ) {
        if self.submit_terminal != Some(DraftSubmitTerminal::OutcomeUnknown) {
            return;
        }
        if self.id != update.modal_id || self.metadata.draft_id.as_ref() != Some(&update.draft_id) {
            return;
        }
        self.error = Some(match error {
            None => {
                "The remote outcome is unknown. The latest text was saved to this draft and copied to the clipboard. Close and do not resend."
                    .to_owned()
            }
            Some(_) => {
                "The remote outcome is unknown. The latest text was copied to the clipboard, but it could not be saved to the draft. Close and do not resend."
                    .to_owned()
            }
        });
    }

    #[cfg(test)]
    pub(crate) fn error_text(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn mark_draft_send_error(&mut self, error: String) {
        self.invalidate_draft_load();
        self.submit_pending = false;
        self.step = FeedbackModalStep::Write;
        self.error = Some(error);
        self.submit_terminal = None;
    }

    pub(crate) fn start_open_draft_list(&mut self) {
        if self.open_on_drafts_if_any {
            self.window.active_tab = FeedbackTab::Drafts.index();
            self.refresh_drafts();
        }
    }

    pub(super) fn refresh_drafts(&mut self) {
        self.invalidate_draft_load();
        self.draft_generation = self.draft_generation.wrapping_add(1);
        let generation = self.draft_generation;
        self.drafts_viewport_start = 0;
        self.drafts = DraftsState::Loading { generation };
        self.pending_request = Some(FeedbackDraftRequest::List {
            modal_id: self.id,
            generation,
        });
    }

    pub(super) fn start_draft_load(&mut self, draft_id: FeedbackDraftId) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
        let load = FeedbackDraftLoad {
            modal_id: self.id,
            generation: self.draft_generation,
            draft_id,
        };
        self.draft_load = Some(load.clone());
        self.pending_request = Some(FeedbackDraftRequest::Load(load));
    }

    pub(super) fn invalidate_draft_load(&mut self) {
        self.draft_load = None;
    }

    pub(super) fn enter_write(&mut self) {
        self.window.active_tab = FeedbackTab::Write.index();
        self.open_on_drafts_if_any = false;
        self.invalidate_draft_load();
    }

    fn set_drafts_error(&mut self, error: String) {
        if let DraftsState::Browse { error: target, .. } = &mut self.drafts {
            *target = Some(error);
        } else {
            self.drafts = DraftsState::Browse {
                rows: Vec::new(),
                selected_id: None,
                query: LineEditor::default(),
                search_focused: false,
                error: Some(error),
            };
        }
    }

    pub(super) fn visible_drafts(&self) -> Vec<&FeedbackDraft> {
        let DraftsState::Browse { rows, query, .. } = &self.drafts else {
            return Vec::new();
        };
        let query = query.text().to_lowercase();
        rows.iter()
            .filter(|draft| {
                if query.is_empty() {
                    return true;
                }
                let mut hay = format!("{} {}", draft.title, draft.details);
                if let Some(value) = draft.r#type {
                    hay.push(' ');
                    hay.push_str(value.label());
                }
                if let Some(value) = draft.task_category {
                    hay.push(' ');
                    hay.push_str(value.label());
                }
                if let Some(value) = draft.failure_mode {
                    hay.push(' ');
                    hay.push_str(value.label());
                }
                hay.to_lowercase().contains(&query)
            })
            .collect()
    }

    fn visible_draft_ids(&self) -> Vec<FeedbackDraftId> {
        self.visible_drafts()
            .into_iter()
            .map(|draft| draft.id.clone())
            .collect()
    }

    pub(super) fn update_drafts_viewport(&mut self, capacity: usize) {
        let visible = self.visible_draft_ids();
        if capacity == 0 || visible.is_empty() {
            self.drafts_viewport_start = 0;
            return;
        }
        let selected = self
            .selected_draft_id()
            .and_then(|id| visible.iter().position(|candidate| *candidate == id))
            .unwrap_or(0);
        let max_start = visible.len().saturating_sub(capacity);
        self.drafts_viewport_start = self.drafts_viewport_start.min(max_start);
        if selected < self.drafts_viewport_start {
            self.drafts_viewport_start = selected;
        } else if selected >= self.drafts_viewport_start + capacity {
            self.drafts_viewport_start = selected + 1 - capacity;
        }
    }

    fn selected_draft_id(&self) -> Option<FeedbackDraftId> {
        let DraftsState::Browse { selected_id, .. } = &self.drafts else {
            return None;
        };
        let visible = self.visible_draft_ids();
        selected_id
            .as_ref()
            .filter(|id| visible.contains(id))
            .cloned()
            .or_else(|| visible.first().cloned())
    }

    pub(super) fn normalize_draft_selection(&mut self) {
        let visible = self.visible_draft_ids();
        if let DraftsState::Browse { selected_id, .. } = &mut self.drafts
            && !selected_id.as_ref().is_some_and(|id| visible.contains(id))
        {
            *selected_id = visible.first().cloned();
        }
    }

    fn move_draft_selection(&mut self, target: DraftMove) {
        let visible = self.visible_draft_ids();
        if visible.is_empty() {
            return;
        }
        let current = self
            .selected_draft_id()
            .and_then(|id| visible.iter().position(|candidate| *candidate == id))
            .unwrap_or(0);
        let index = match target {
            DraftMove::Next => (current + 1).min(visible.len() - 1),
            DraftMove::Previous => current.saturating_sub(1),
            DraftMove::First => 0,
            DraftMove::Last => visible.len() - 1,
        };
        if let DraftsState::Browse { selected_id, .. } = &mut self.drafts {
            *selected_id = visible.get(index).cloned();
        }
    }

    /// Re-snapshot after draft images are attached so a clean load is not dirty.
    pub(crate) fn recapture_write_baseline(&mut self) {
        self.capture_write_baseline();
    }

    fn capture_write_baseline(&mut self) {
        self.write_baseline = (
            self.text().to_owned(),
            self.draft_title.clone(),
            self.draft_area.clone(),
            self.metadata.clone(),
            self.composer
                .images
                .iter()
                .map(|image| image.element_id)
                .collect(),
        );
    }

    fn has_unsaved_write(&self) -> bool {
        let image_ids: Vec<_> = self
            .composer
            .images
            .iter()
            .map(|image| image.element_id)
            .collect();
        let differs_from_baseline = self.write_baseline.0 != self.text()
            || self.write_baseline.1 != self.draft_title
            || self.write_baseline.2 != self.draft_area
            || self.write_baseline.3.r#type != self.metadata.r#type
            || self.write_baseline.3.task_category != self.metadata.task_category
            || self.write_baseline.3.failure_mode != self.metadata.failure_mode
            || self.write_baseline.4 != image_ids;
        if self.metadata.draft_id.is_some() {
            differs_from_baseline
        } else {
            differs_from_baseline
                || !self.text().is_empty()
                || !image_ids.is_empty()
                || self.metadata.r#type.is_some()
                || self.metadata.task_category.is_some()
                || self.metadata.failure_mode.is_some()
        }
    }

    pub(super) fn activate_drafts_tab(&mut self) {
        self.window.active_tab = FeedbackTab::Drafts.index();
        self.open_on_drafts_if_any = false;
        if self.draft_delete.is_none() {
            self.refresh_drafts();
        }
    }

    fn select_draft(&mut self, draft_id: FeedbackDraftId) {
        if self.has_unsaved_write() {
            self.discard_confirm = Some(draft_id);
            return;
        }
        self.start_draft_load(draft_id);
    }

    pub(super) fn handle_draft_click(&mut self, column: u16, row: u16) {
        if self
            .draft_search_area
            .is_some_and(|area| area.contains((column, row).into()))
        {
            if let DraftsState::Browse { search_focused, .. } = &mut self.drafts {
                *search_focused = true;
            }
            self.last_draft_click = None;
            return;
        }
        let Some(draft_id) = self.draft_row_areas.iter().find_map(|(draft_id, area)| {
            area.contains((column, row).into())
                .then(|| draft_id.clone())
        }) else {
            self.last_draft_click = None;
            return;
        };
        if let DraftsState::Browse { selected_id, .. } = &mut self.drafts {
            *selected_id = Some(draft_id.clone());
        }
        let now = std::time::Instant::now();
        let is_double_click = self
            .last_draft_click
            .as_ref()
            .is_some_and(|(last, previous_id)| {
                *previous_id == draft_id && now.duration_since(*last) < DRAFT_DOUBLE_CLICK_TIMEOUT
            });
        if is_double_click {
            self.last_draft_click = None;
            self.select_draft(draft_id);
        } else {
            self.last_draft_click = Some((now, draft_id));
        }
    }

    pub(super) fn handle_drafts_key(&mut self, key: &KeyEvent) -> FeedbackModalOutcome {
        if self.submit_pending || self.is_draft_submit_terminal() {
            return FeedbackModalOutcome::Changed;
        }
        if self.draft_delete.is_some() || self.draft_load.is_some() {
            return FeedbackModalOutcome::Changed;
        }
        if let Some(draft_id) = self.discard_confirm.clone() {
            match key.code {
                KeyCode::Char('y') if key.modifiers.is_empty() => {
                    self.discard_confirm = None;
                    self.start_draft_load(draft_id);
                }
                KeyCode::Char('n') | KeyCode::Esc => self.discard_confirm = None,
                _ => {}
            }
            return FeedbackModalOutcome::Changed;
        }
        if let Some(draft_id) = self.delete_confirm.clone() {
            match key.code {
                KeyCode::Char('y') if key.modifiers.is_empty() => {
                    self.next_draft_delete_token = self.next_draft_delete_token.wrapping_add(1);
                    let delete = FeedbackDraftDelete {
                        modal_id: self.id,
                        token: FeedbackDraftDeleteToken(self.next_draft_delete_token),
                        draft_id,
                    };
                    self.draft_delete = Some(delete.clone());
                    self.pending_request = Some(FeedbackDraftRequest::Delete(delete));
                }
                KeyCode::Char('n') | KeyCode::Esc => self.delete_confirm = None,
                _ => {}
            }
            return FeedbackModalOutcome::Changed;
        }
        if matches!(
            self.drafts,
            DraftsState::Browse {
                search_focused: true,
                ..
            }
        ) {
            match key.code {
                KeyCode::Enter => {
                    if let Some(draft_id) = self.selected_draft_id() {
                        self.select_draft(draft_id);
                    }
                }
                KeyCode::Esc => {
                    let has_query = matches!(
                        &self.drafts,
                        DraftsState::Browse { query, .. } if !query.text().is_empty()
                    );
                    if let DraftsState::Browse {
                        query,
                        search_focused,
                        ..
                    } = &mut self.drafts
                    {
                        if has_query {
                            query.reset();
                        } else {
                            *search_focused = false;
                        }
                    }
                }
                KeyCode::Down => self.move_draft_selection(DraftMove::Next),
                KeyCode::Up => self.move_draft_selection(DraftMove::Previous),
                _ => {
                    if let DraftsState::Browse { query, .. } = &mut self.drafts {
                        let _ = query.handle_key_with_insert_policy(key, |character| {
                            !crate::render::line_utils::is_unsafe_display_char(character)
                        });
                    }
                }
            }
            self.normalize_draft_selection();
            return FeedbackModalOutcome::Changed;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.enter_write();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_draft_selection(DraftMove::Next),
            KeyCode::Up | KeyCode::Char('k') => self.move_draft_selection(DraftMove::Previous),
            KeyCode::Char('g') if key.modifiers.is_empty() => {
                self.move_draft_selection(DraftMove::First)
            }
            KeyCode::Char('G') if key.modifiers.is_empty() => {
                self.move_draft_selection(DraftMove::Last)
            }
            KeyCode::Char('/') | KeyCode::Char('i') if key.modifiers.is_empty() => {
                if let DraftsState::Browse { search_focused, .. } = &mut self.drafts {
                    *search_focused = true;
                }
            }
            KeyCode::Enter => {
                if let Some(draft_id) = self.selected_draft_id() {
                    self.select_draft(draft_id);
                }
            }
            KeyCode::Char('d') | KeyCode::Delete if key.modifiers.is_empty() => {
                self.delete_confirm = self.selected_draft_id();
            }
            KeyCode::Esc => return FeedbackModalOutcome::Cancel,
            _ => {}
        }
        FeedbackModalOutcome::Changed
    }
}
