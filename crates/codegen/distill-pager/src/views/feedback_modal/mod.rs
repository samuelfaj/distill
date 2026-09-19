// Modified for Distill by Samuel Fajreldines, 2026.
//! Isolated feedback editor rendered with the shared modal chrome.
//!
//! Feedback taxonomy declaration order is the fixed cycle/picker order.

use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

pub use distill_feedback::{
    FeedbackDraft, FeedbackDraftId, FeedbackFailureMode, FeedbackTaskCategory, FeedbackTaxonomy,
    FeedbackType,
};
use distill_feedback::{FeedbackSource, structured_feedback};
pub use distill_shell::session::FeedbackTraceUploadIntent;

use crate::views::modal_window::{self, ModalWindowOutcome, ModalWindowState};
use crate::views::prompt_widget::{EnterOutcome, FeedbackImages, PromptEvent, PromptWidget};

mod drafts;
mod enum_picker;
mod render;
use drafts::DraftSubmitTerminal;
pub use drafts::{
    DraftsState, FeedbackDraftDelete, FeedbackDraftDeleteToken, FeedbackDraftLoad,
    FeedbackDraftRequest, FeedbackDraftUpdate,
};
use enum_picker::EnumPicker;

static NEXT_FEEDBACK_MODAL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FEEDBACK_SUBMISSION_ID: AtomicU64 = AtomicU64::new(1);
const FEEDBACK_TABS: &[&str] = &["Write", "Drafts"];
// One id shared by the footer `Shortcut` and the mouse `ShortcutActivated` arm; drifting them breaks click-to-cancel.
const CANCEL_SHORTCUT_ID: usize = 1;
const DRAFT_DOUBLE_CLICK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);
/// One copy of the empty-submit refusal: the key layer and the submit dispatcher must not drift apart.
pub(crate) const FEEDBACK_EMPTY_SUBMIT_ERROR: &str =
    "Add feedback text or an image before sending.";
/// Open-generation identity: a deferred paste or submit completion applies only to the same still-open modal, never a reopened one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeedbackModalId(u64);

/// Write-buffer generation: a deferred paste applies only to the composition that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeedbackCompositionId(u64);

/// Title, area, and taxonomy ready to send from a loaded draft. Type is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackDraftSendFields {
    pub title: String,
    pub area: Option<String>,
    pub r#type: FeedbackType,
    pub task_category: Option<FeedbackTaskCategory>,
    pub failure_mode: Option<FeedbackFailureMode>,
}

/// Allocated per POST attempt (separate from the modal's open generation), so a stale completion cannot act on a later attempt's parked consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeedbackSubmissionId(u64);

impl FeedbackSubmissionId {
    /// Allocate the identity for one POST attempt.
    pub(crate) fn next() -> Self {
        Self(NEXT_FEEDBACK_SUBMISSION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// One-shot consent committed with a modal POST. The modal closes at submit, so this parks on the
/// agent until the matching successful completion takes it; failure drops it, and nothing else reads it.
#[derive(Debug)]
pub(crate) struct ParkedFeedbackTraceConsent {
    pub(crate) intent: FeedbackTraceUploadIntent,
    /// Session id used for the POST. The upload must not re-read the live session.
    pub(crate) session_id: String,
}

/// Which mandatory surface preempted an open feedback modal (see [`crate::app::agent_view::AgentView::displace_feedback_modal`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackModalDisplacement {
    CancelTurn,
    PlanApproval,
    Permission,
    AcpQuestion,
    LocalQuestion,
    McpElicitation,
    HookBlockedPrompt,
}

impl FeedbackModalDisplacement {
    pub(crate) fn notice(self) -> &'static str {
        match self {
            Self::CancelTurn => "Feedback closed because the turn-cancel prompt needs an answer.",
            Self::PlanApproval => "Feedback closed because a plan is ready for approval.",
            Self::Permission => "Feedback closed because a permission request needs an answer.",
            Self::AcpQuestion => "Feedback closed because the agent asked a question.",
            Self::LocalQuestion => "Feedback closed because another prompt needs an answer.",
            Self::McpElicitation => "Feedback closed because a tool needs your input.",
            Self::HookBlockedPrompt => "Feedback closed because a hook blocked the prompt.",
        }
    }
}

/// What the user picked on the in-modal trace step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackTraceChoice {
    /// One archive for this successfully posted report; never a persisted grant.
    SendThisSession,
    /// Send the report alone.
    FeedbackOnly,
    /// Send the report alone and persist `[features] feedback_trace_card = false`.
    NeverAsk,
}

impl FeedbackTraceChoice {
    /// Fixed render order; index math in the key handler assumes it.
    pub(crate) const ALL: [FeedbackTraceChoice; 3] =
        [Self::SendThisSession, Self::FeedbackOnly, Self::NeverAsk];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SendThisSession => "Send this session's trace",
            Self::FeedbackOnly => "No, just the feedback",
            Self::NeverAsk => "No, and don't ask again",
        }
    }
}

#[derive(Debug, Default)]
pub struct OpenFeedbackModal {
    pub text: Option<String>,
    pub r#type: Option<FeedbackType>,
    pub task_category: Option<FeedbackTaskCategory>,
    pub failure_mode: Option<FeedbackFailureMode>,
    pub images: FeedbackImages,
    pub draft_id: Option<FeedbackDraftId>,
}

#[derive(Debug, Clone)]
pub(crate) struct FeedbackModalMetadata {
    r#type: Option<FeedbackType>,
    task_category: Option<FeedbackTaskCategory>,
    failure_mode: Option<FeedbackFailureMode>,
    draft_id: Option<FeedbackDraftId>,
}

/// One editable enum slot on the Write step's label row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataField {
    Type,
    Task,
    Failure,
}

impl FeedbackModalMetadata {
    /// The fields the open payload supplied, in the default render order; the state's `field_order`
    /// seeds from this once per open and never changes after. An absent enum never grows an empty row,
    /// so bare opens keep a neutral Write step.
    fn present_fields(&self) -> Vec<MetadataField> {
        let mut fields = Vec::new();
        if self.r#type.is_some() {
            fields.push(MetadataField::Type);
        }
        if self.task_category.is_some() {
            fields.push(MetadataField::Task);
        }
        if self.failure_mode.is_some() {
            fields.push(MetadataField::Failure);
        }
        fields
    }

    /// Draft send needs type and task rows even when the stored draft omitted them.
    fn draft_editor_fields(&self) -> Vec<MetadataField> {
        let mut fields = vec![MetadataField::Type, MetadataField::Task];
        if self.failure_mode.is_some() {
            fields.push(MetadataField::Failure);
        }
        fields
    }

    fn field_text(&self, field: MetadataField) -> Option<String> {
        match field {
            MetadataField::Type => Some(match &self.r#type {
                Some(value) => format!("Type: {}", value.label()),
                None if self.draft_id.is_some() => "Type: (choose)".to_string(),
                None => return None,
            }),
            MetadataField::Task => Some(match &self.task_category {
                Some(value) => format!("Task: {}", value.label()),
                None if self.draft_id.is_some() => "Task: (choose)".to_string(),
                None => return None,
            }),
            MetadataField::Failure => self
                .failure_mode
                .as_ref()
                .map(|value| format!("Failure: {}", value.label())),
        }
    }

    /// The typed enums as the versioned `structured_feedback` envelope for the POST's `metadata`
    /// bag: fixed wire values only, never user-authored text. Absent enums are omitted (no JSON
    /// null); the envelope itself always goes out so every send carries its `source`.
    fn structured_feedback(&self) -> serde_json::Value {
        let source = if self.draft_id.is_some() {
            FeedbackSource::Draft
        } else {
            FeedbackSource::Write
        };
        structured_feedback(
            source,
            FeedbackTaxonomy {
                r#type: self.r#type,
                task_category: self.task_category,
                failure_mode: self.failure_mode,
            },
        )
    }
}

/// The modal's flow position. Write -> (Trace when the dispatcher offers it);
/// a committed submit closes the modal instead of holding a Sending state.
enum FeedbackModalStep {
    Write,
    Trace {
        /// The highlighted option.
        selected: FeedbackTraceChoice,
        /// Set only by Enter: the submit dispatcher sends nothing from an undecided Trace step,
        /// so a stray replayed submit cannot commit whatever happens to be highlighted.
        decided: Option<FeedbackTraceChoice>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackTab {
    Write,
    Drafts,
}

impl FeedbackTab {
    pub(super) fn index(self) -> usize {
        match self {
            Self::Write => 0,
            Self::Drafts => 1,
        }
    }

    fn from_index(index: usize) -> Self {
        if index == 1 {
            Self::Drafts
        } else {
            Self::Write
        }
    }
}

pub struct FeedbackModalState {
    pub window: ModalWindowState,
    id: FeedbackModalId,
    composer: PromptWidget,
    metadata: FeedbackModalMetadata,
    /// Which supplied enum's label row is focused; `None` keeps the composer as the key owner.
    metadata_focus: Option<MetadataField>,
    /// Stable display order of the supplied enums' rows, seeded once at open; membership and
    /// order never change after (Left/Right cycle a row's value, never its position). Render,
    /// key navigation, and hit-testing all read this, never `present_fields`.
    field_order: Vec<MetadataField>,
    /// Label-row rects from the last render, for click-to-focus; empty whenever no rows drew.
    metadata_row_areas: Vec<(MetadataField, Rect)>,
    /// Open only from Enter on a focused label row; owns the keys until commit (Enter) or back-out (Esc).
    enum_picker: Option<EnumPicker>,
    step: FeedbackModalStep,
    composition_id: FeedbackCompositionId,
    /// Deferred clipboard-attachment probes this composition started; a submit while any are in flight is deferred, not dropped.
    paste_probes_in_flight: usize,
    image_rehydrations_in_flight: usize,
    deferred_submit: bool,
    trace_outcome_reported: bool,
    error: Option<String>,
    drafts: DraftsState,
    /// Bare `/feedback` with an empty Write form starts on Drafts when a list comes back nonempty.
    /// Any user tab change clears it so a late list never moves them.
    open_on_drafts_if_any: bool,
    draft_generation: u64,
    draft_load: Option<FeedbackDraftLoad>,
    pending_request: Option<FeedbackDraftRequest>,
    delete_confirm: Option<FeedbackDraftId>,
    discard_confirm: Option<FeedbackDraftId>,
    draft_delete: Option<FeedbackDraftDelete>,
    next_draft_delete_token: u64,
    submit_terminal: Option<DraftSubmitTerminal>,
    submit_pending: bool,
    draft_title: Option<String>,
    draft_area: Option<String>,
    write_baseline: (
        String,
        Option<String>,
        Option<String>,
        FeedbackModalMetadata,
        Vec<distill_ratatui_textarea::ElementId>,
    ),
    drafts_viewport_start: usize,
    draft_search_area: Option<Rect>,
    draft_row_areas: Vec<(FeedbackDraftId, Rect)>,
    last_draft_click: Option<(std::time::Instant, FeedbackDraftId)>,
}

impl Drop for FeedbackModalState {
    fn drop(&mut self) {
        self.report_dismissed_trace_card();
        self.composer.reconcile_feedback_images_on_teardown();
        self.composer.preserve_pending_feedback_image_sources();
        self.composer.cleanup_images_on_teardown();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackModalOutcome {
    Changed,
    Cancel,
    Submit,
}

/// A successful render's hand-back to `AgentView`: the composer's terminal caret and any post-flush escapes.
/// `cursor` is the hardware cursor position; the textarea draws no caret cell of its own, so dropping it leaves the Write step caret-less.
pub struct FeedbackModalRender {
    pub cursor: Option<(u16, u16)>,
    pub post_flush: Option<crate::terminal::overlay::PostFlush>,
}

impl FeedbackModalState {
    pub fn new(mut open: OpenFeedbackModal) -> Self {
        let open_on_drafts_if_any = open
            .text
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
            && open.images.is_empty()
            && open.draft_id.is_none();
        let id = FeedbackModalId(NEXT_FEEDBACK_MODAL_ID.fetch_add(1, Ordering::Relaxed));
        let mut composer = PromptWidget::new();
        composer.disable_file_search();
        let text = open.text.take();
        let images = open.images.take();
        if let Some(text) = text {
            composer.set_text(&text);
            composer.set_cursor(text.len());
        }
        let rejected_images = composer.seed_images(images);
        let image_rehydrations_in_flight = composer
            .images
            .iter()
            .filter(|image| image.encoded_bytes.is_none() && image.session_image_path.is_some())
            .count();
        let metadata = FeedbackModalMetadata {
            r#type: open.r#type,
            task_category: open.task_category,
            failure_mode: open.failure_mode,
            draft_id: open.draft_id,
        };
        let field_order = metadata.present_fields();
        let write_baseline = (
            composer.text().to_owned(),
            None,
            None,
            metadata.clone(),
            composer
                .images
                .iter()
                .map(|image| image.element_id)
                .collect(),
        );
        Self {
            window: ModalWindowState::with_tabs(FEEDBACK_TABS.len()),
            id,
            composer,
            metadata,
            metadata_focus: None,
            field_order,
            metadata_row_areas: Vec::new(),
            enum_picker: None,
            step: FeedbackModalStep::Write,
            composition_id: FeedbackCompositionId(1),
            paste_probes_in_flight: 0,
            image_rehydrations_in_flight,
            deferred_submit: false,
            trace_outcome_reported: false,
            error: (rejected_images > 0)
                .then(|| format!("Dropped {rejected_images} invalid image(s).")),
            drafts: DraftsState::Unloaded,
            open_on_drafts_if_any,
            draft_generation: 0,
            draft_load: None,
            pending_request: None,
            delete_confirm: None,
            discard_confirm: None,
            draft_delete: None,
            next_draft_delete_token: 0,
            submit_terminal: None,
            submit_pending: false,
            draft_title: None,
            draft_area: None,
            write_baseline,
            drafts_viewport_start: 0,
            draft_search_area: None,
            draft_row_areas: Vec::new(),
            last_draft_click: None,
        }
    }

    pub(crate) fn id(&self) -> FeedbackModalId {
        self.id
    }

    pub(crate) fn matches_id(&self, id: FeedbackModalId) -> bool {
        self.id == id
    }

    pub(crate) fn composition_id(&self) -> FeedbackCompositionId {
        self.composition_id
    }

    pub(crate) fn matches_composition(&self, id: FeedbackCompositionId) -> bool {
        self.composition_id == id
    }

    pub fn text(&self) -> &str {
        self.composer.text()
    }

    pub fn active_tab(&self) -> FeedbackTab {
        FeedbackTab::from_index(self.window.active_tab)
    }

    pub fn draft_id(&self) -> Option<&FeedbackDraftId> {
        self.metadata.draft_id.as_ref()
    }

    pub(crate) fn draft_body(&self) -> Option<FeedbackDraftSendFields> {
        let title = self.draft_title.clone()?;
        let r#type = self.metadata.r#type?;
        Some(FeedbackDraftSendFields {
            title,
            area: self.draft_area.clone(),
            r#type,
            task_category: self.metadata.task_category,
            failure_mode: self.metadata.failure_mode,
        })
    }

    #[cfg(test)]
    fn is_draft_submit_unknown(&self) -> bool {
        self.submit_terminal == Some(DraftSubmitTerminal::OutcomeUnknown)
    }

    pub(crate) fn is_draft_submit_pending(&self) -> bool {
        self.submit_pending && self.metadata.draft_id.is_some()
    }

    fn is_draft_submit_terminal(&self) -> bool {
        self.submit_terminal.is_some()
    }

    /// POST body: composer buffer with `[Image #N]` chip placeholders removed.
    pub fn submitted_text(&self) -> String {
        self.composer.text_without_image_chips()
    }

    pub fn image_count(&self) -> usize {
        self.composer.images.len()
    }

    /// Move composer chips into an already-open modal. Call only after the open
    /// was accepted; a refusal must leave the composer untouched.
    pub(crate) fn absorb_composer_images(
        &mut self,
        images: Vec<crate::prompt_images::PastedImage>,
    ) -> Vec<(u64, std::path::PathBuf)> {
        if images.is_empty() {
            return Vec::new();
        }
        let before = self.composer.images.len();
        let _rejected = self.composer.seed_images(images);
        // Composer chips make this a new report, not a bare open: stay on Write.
        if self.open_on_drafts_if_any {
            self.window.active_tab = FeedbackTab::Write.index();
            self.open_on_drafts_if_any = false;
        }
        let requests = self
            .composer
            .images
            .get(before..)
            .unwrap_or(&[])
            .iter()
            .filter(|image| image.encoded_bytes.is_none())
            .filter_map(|image| {
                image
                    .session_image_path
                    .clone()
                    .map(|path| (image.preview.identity(), path))
            })
            .collect::<Vec<_>>();
        self.image_rehydrations_in_flight = self
            .image_rehydrations_in_flight
            .saturating_add(requests.len());
        requests
    }

    pub(crate) fn image_rehydration_requests(&self) -> Vec<(u64, std::path::PathBuf)> {
        self.composer
            .images
            .iter()
            .filter(|image| image.encoded_bytes.is_none())
            .filter_map(|image| {
                image
                    .session_image_path
                    .clone()
                    .map(|path| (image.preview.identity(), path))
            })
            .collect()
    }

    /// Feedback is sendable with nonblank text or at least one live image chip.
    pub fn is_sendable(&self) -> bool {
        !self.composer.text_without_image_chips().trim().is_empty()
            || self.composer.has_live_image()
    }

    /// Swap Write for the in-modal trace question. The draft stays in the composer untouched.
    /// Production entry is the submit dispatcher.
    pub(crate) fn begin_trace_step(&mut self) {
        self.invalidate_draft_load();
        self.step = FeedbackModalStep::Trace {
            // FeedbackOnly is the non-permissive default so a second Enter (double-tap
            // or key-repeat from Write submit) cannot grant a one-shot trace upload.
            selected: FeedbackTraceChoice::FeedbackOnly,
            decided: None,
        };
        // Backing out of the trace step must land in the composer, not a stale label focus or picker.
        self.metadata_focus = None;
        self.enum_picker = None;
        self.trace_outcome_reported = false;
        self.error = None;
    }

    fn report_trace_outcome(
        &mut self,
        choice: distill_telemetry::events::FeedbackTraceConsentChoice,
    ) {
        if !self.trace_outcome_reported {
            distill_telemetry::session_ctx::log_event(
                distill_telemetry::events::FeedbackTraceConsentSelected {
                    choice,
                    reenables_sharing: false,
                },
            );
            self.trace_outcome_reported = true;
        }
    }

    pub(crate) fn report_confirmed_trace_choice(&mut self, choice: FeedbackTraceChoice) {
        let choice = match choice {
            FeedbackTraceChoice::SendThisSession => {
                distill_telemetry::events::FeedbackTraceConsentChoice::SendThisSession
            }
            FeedbackTraceChoice::FeedbackOnly => {
                distill_telemetry::events::FeedbackTraceConsentChoice::NoUpload
            }
            FeedbackTraceChoice::NeverAsk => {
                distill_telemetry::events::FeedbackTraceConsentChoice::NeverAsk
            }
        };
        self.report_trace_outcome(choice);
    }

    fn report_dismissed_trace_card(&mut self) {
        if self.in_trace_step() {
            self.report_trace_outcome(
                distill_telemetry::events::FeedbackTraceConsentChoice::Dismissed,
            );
        }
    }

    pub fn blocks_composer_input(&self) -> bool {
        self.active_tab() != FeedbackTab::Write
            || self.submit_pending
            || self.draft_load.is_some()
            || self.in_trace_step()
            || self.is_draft_submit_terminal()
    }

    pub fn in_trace_step(&self) -> bool {
        matches!(self.step, FeedbackModalStep::Trace { .. })
    }

    pub(crate) fn decided_trace_choice(&self) -> Option<FeedbackTraceChoice> {
        match self.step {
            FeedbackModalStep::Trace { decided, .. } => decided,
            FeedbackModalStep::Write => None,
        }
    }

    /// Consume the Enter-confirmed trace choice, exactly once per confirmation.
    /// The production consumer is the submit dispatcher.
    pub(crate) fn take_decided_trace_choice(&mut self) -> Option<FeedbackTraceChoice> {
        match &mut self.step {
            FeedbackModalStep::Trace { decided, .. } => decided.take(),
            _ => None,
        }
    }

    /// Trace-step question copy comes only from the supplied feedback type;
    /// an absent type uses the neutral line, and user-authored text never picks it.
    fn trace_prompt(&self) -> &'static str {
        match self.metadata.r#type {
            Some(FeedbackType::Bug) => "Attach this session's trace to help us debug this bug?",
            Some(FeedbackType::Idea) => "Attach this session's trace to give this idea context?",
            Some(FeedbackType::MissingCapability) => {
                "Attach this session's trace to show what was missing?"
            }
            None => "Attach this session's trace to your feedback?",
        }
    }

    pub(crate) fn set_error(&mut self, error: String) {
        self.invalidate_draft_load();
        self.error = Some(error);
    }

    /// Replace one pending disk-backed attachment after its bounded blocking read completes.
    pub(crate) fn apply_rehydrated_image(
        &mut self,
        image_identity: u64,
        result: Result<Vec<u8>, String>,
    ) {
        self.image_rehydrations_in_flight = self.image_rehydrations_in_flight.saturating_sub(1);
        let Ok(bytes) = result else {
            self.composer
                .drop_image_preserving_session_file(image_identity);
            self.error = Some(
                "Couldn't restore one feedback image; the original file was kept.".to_string(),
            );
            return;
        };
        let Some(image) = self
            .composer
            .images
            .iter_mut()
            .find(|image| image.preview.identity() == image_identity)
        else {
            return;
        };
        let Some(path) = image.session_image_path.take() else {
            return;
        };
        image.byte_len = bytes.len();
        image.encoded_bytes = Some(bytes.into());
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "feedback image source cleanup failed");
        }
    }

    /// The (possibly edited) enums a committed send carries in the POST's metadata bag, as the
    /// versioned `structured_feedback` envelope (always present, with or without enums).
    pub(crate) fn structured_feedback_metadata(&self) -> serde_json::Value {
        self.metadata.structured_feedback()
    }

    pub(crate) fn reconcile_feedback_images(&mut self) {
        self.composer.reconcile_feedback_images_on_teardown();
    }

    pub(crate) fn images(&self) -> &[crate::prompt_images::PastedImage] {
        &self.composer.images
    }

    /// Transfer committed attachments out of the throwaway modal owner.
    pub(crate) fn take_images(&mut self) -> FeedbackImages {
        self.composer.reconcile_feedback_images_on_teardown();
        self.composer.drain_images().into()
    }

    pub(crate) fn insert_image(
        &mut self,
        image: crate::prompt_images::PastedImage,
    ) -> Result<(), String> {
        self.invalidate_draft_load();
        self.composer.insert_image(image)
    }

    pub(crate) fn note_paste_probe_started(&mut self) {
        self.invalidate_draft_load();
        self.paste_probes_in_flight += 1;
    }

    pub(crate) fn note_paste_probe_finished(&mut self) {
        self.paste_probes_in_flight = self.paste_probes_in_flight.saturating_sub(1);
    }

    /// Consume the submit deferred behind this modal's paste probes once they have all completed.
    pub(crate) fn take_deferred_submit(&mut self) -> bool {
        if self.paste_probes_in_flight != 0
            || self.image_rehydrations_in_flight != 0
            || !self.deferred_submit
        {
            return false;
        }
        self.deferred_submit = false;
        true
    }

    pub(crate) fn cancel_deferred_submit(&mut self) {
        self.deferred_submit = false;
    }

    #[cfg(test)]
    fn composer_mut(&mut self) -> &mut PromptWidget {
        &mut self.composer
    }

    #[cfg(test)]
    fn metadata(&self) -> &FeedbackModalMetadata {
        &self.metadata
    }

    #[cfg(test)]
    fn enum_picker_open(&self) -> bool {
        self.enum_picker.is_some()
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> FeedbackModalOutcome {
        if self.active_tab() == FeedbackTab::Drafts {
            return self.handle_drafts_key(key);
        }
        self.invalidate_draft_load();
        if self.submit_pending {
            return FeedbackModalOutcome::Changed;
        }
        if self.submit_terminal.is_some() {
            return match key.code {
                KeyCode::Esc => FeedbackModalOutcome::Cancel,
                _ => FeedbackModalOutcome::Changed,
            };
        }
        if self.draft_load.is_some() {
            return match key.code {
                KeyCode::Esc => FeedbackModalOutcome::Cancel,
                _ => FeedbackModalOutcome::Changed,
            };
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            && self.enum_picker.is_none()
            && self.metadata_focus.is_none()
        {
            self.activate_drafts_tab();
            return FeedbackModalOutcome::Changed;
        }
        if let FeedbackModalStep::Trace { selected, .. } = &self.step {
            let selected = *selected;
            return self.handle_trace_key(key, selected);
        }
        if self.enum_picker.is_some() {
            return self.handle_enum_picker_event(&crossterm::event::Event::Key(*key));
        }
        if let Some(field) = self.metadata_focus {
            if let Some(outcome) = self.handle_metadata_key(key, field) {
                return outcome;
            }
        } else if key.code == KeyCode::Tab
            // Focus only rows the last render actually drew (`metadata_row_areas` is emptied
            // when the rows drop): focusing an invisible row would just hide the caret.
            && let Some((first, _)) = self.metadata_row_areas.first().copied()
        {
            self.metadata_focus = Some(first);
            return FeedbackModalOutcome::Changed;
        } else if key.code == KeyCode::Up
            && key.modifiers.is_empty()
            // An open prompt-anchored dropdown owns Up (result navigation).
            && !self.composer.any_dropdown_open()
            // Wrap-aware top: a long wrapped draft keeps Up for caret movement until the
            // caret reaches the first visual row.
            && self.composer.caret_on_top_visual_row()
            && let Some((last, _)) = self.metadata_row_areas.last().copied()
        {
            // Up at the top of the draft climbs onto the drawn label block; the bottom row is adjacent.
            self.metadata_focus = Some(last);
            return FeedbackModalOutcome::Changed;
        }
        let config = Self::window_config(&[], false, false);
        match modal_window::handle_modal_key(&mut self.window, key, &config) {
            ModalWindowOutcome::CloseRequested => return FeedbackModalOutcome::Cancel,
            ModalWindowOutcome::Unhandled => {}
            _ => return FeedbackModalOutcome::Changed,
        }
        match self.composer.route_enter(key) {
            EnterOutcome::NewlineInserted => {
                self.error = None;
                return FeedbackModalOutcome::Changed;
            }
            EnterOutcome::Submit if self.is_sendable() => {
                if self.paste_probes_in_flight > 0 || self.image_rehydrations_in_flight > 0 {
                    // A screenshot is still attaching; send with it once the blocking work settles.
                    self.deferred_submit = true;
                    return FeedbackModalOutcome::Changed;
                }
                return FeedbackModalOutcome::Submit;
            }
            EnterOutcome::Submit => {
                self.error = Some(FEEDBACK_EMPTY_SUBMIT_ERROR.to_string());
                return FeedbackModalOutcome::Changed;
            }
            EnterOutcome::PassThrough => {}
        }
        match self.composer.handle_key(key) {
            PromptEvent::Edited => {
                self.error = None;
                FeedbackModalOutcome::Changed
            }
            PromptEvent::Ignored => FeedbackModalOutcome::Changed,
        }
    }

    /// The focused label rows are a vertical list, not a composer: Up/Down move among the rows. `None`
    /// declines the key so typing falls through to the composer (with focus returned), keeping the
    /// draft reachable without an extra Tab.
    fn handle_metadata_key(
        &mut self,
        key: &KeyEvent,
        field: MetadataField,
    ) -> Option<FeedbackModalOutcome> {
        match key.code {
            // Esc here only drops the row focus; cancelling stays a composer-focused Esc.
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => {
                self.metadata_focus = None;
            }
            // Left/Right edit the focused field's value in place, never move focus (Up/Down):
            // Left steps to the previous variant, Right to the next, both wrapping.
            KeyCode::Left => self.cycle_metadata_field(field, false),
            KeyCode::Right => self.cycle_metadata_field(field, true),
            // The rows sit stacked above the composer, so Up/Down never wrap: Up stops at the
            // top row and Down off the bottom row lands back in the composer, mirroring the
            // Up-from-the-draft-top entry.
            KeyCode::Up => {
                if let Some(index) = self.field_order.iter().position(|f| *f == field)
                    && index > 0
                {
                    self.metadata_focus = index
                        .checked_sub(1)
                        .and_then(|i| self.field_order.get(i))
                        .copied();
                }
            }
            KeyCode::Down => {
                let index = self
                    .field_order
                    .iter()
                    .position(|f| *f == field)
                    .unwrap_or(0);
                self.metadata_focus = self.field_order.get(index + 1).copied();
            }
            // Enter opens the picker instead of submitting so a send always happens from the composer.
            KeyCode::Enter => self.open_enum_picker(field),
            _ => {
                self.metadata_focus = None;
                return None;
            }
        }
        Some(FeedbackModalOutcome::Changed)
    }

    /// Step the focused field's value to the previous (`forward == false`) or next variant in
    /// its fixed declaration order, wrapping at the ends. Only the value changes: the row order
    /// and the focus stay put, exactly like an Enter-picker commit of the neighboring variant.
    fn cycle_metadata_field(&mut self, field: MetadataField, forward: bool) {
        let len = field.variant_labels().len();
        if len == 0 {
            return;
        }
        let index = self.metadata.field_variant_index(field);
        let next = if forward {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        self.metadata.set_field_variant(field, next);
    }

    /// The trace step is a list, not a composer: navigation moves the highlight and Enter confirms it.
    fn handle_trace_key(
        &mut self,
        key: &KeyEvent,
        selected: FeedbackTraceChoice,
    ) -> FeedbackModalOutcome {
        self.error = None;
        let options = FeedbackTraceChoice::ALL;
        let index = options
            .iter()
            .position(|choice| *choice == selected)
            .unwrap_or(0);
        let next = match key.code {
            // Esc backs out to Write without sending; the draft stays and a second Esc cancels from there.
            KeyCode::Esc => {
                self.report_dismissed_trace_card();
                self.step = FeedbackModalStep::Write;
                return FeedbackModalOutcome::Changed;
            }
            KeyCode::Up => options
                .get((index + options.len() - 1) % options.len())
                .copied()
                .unwrap_or(selected),
            KeyCode::Down => options
                .get((index + 1) % options.len())
                .copied()
                .unwrap_or(selected),
            KeyCode::Char(digit @ '1'..='3') => options
                .get(digit as usize - '1' as usize)
                .copied()
                .unwrap_or(selected),
            KeyCode::Enter => {
                self.step = FeedbackModalStep::Trace {
                    selected,
                    decided: Some(selected),
                };
                return FeedbackModalOutcome::Submit;
            }
            _ => return FeedbackModalOutcome::Changed,
        };
        self.step = FeedbackModalStep::Trace {
            selected: next,
            decided: None,
        };
        FeedbackModalOutcome::Changed
    }

    pub fn handle_paste(&mut self, text: &str) -> FeedbackModalOutcome {
        if self.active_tab() == FeedbackTab::Drafts {
            let was_inserted = if let DraftsState::Browse {
                query,
                search_focused: true,
                ..
            } = &mut self.drafts
            {
                query.insert_paste(text) == crate::input::line_editor::LineEditOutcome::TextChanged
            } else {
                false
            };
            if was_inserted {
                self.normalize_draft_selection();
            }
            return FeedbackModalOutcome::Changed;
        }
        self.invalidate_draft_load();
        // The trace step has no focused composer; pasting into the hidden draft would be invisible.
        if self.blocks_composer_input() {
            return FeedbackModalOutcome::Changed;
        }
        // An open picker owns input: the paste filters its list, never the hidden draft.
        if self.enum_picker.is_some() {
            return self
                .handle_enum_picker_event(&crossterm::event::Event::Paste(text.to_string()));
        }
        // A paste edits the draft, so the composer takes focus back from the label row.
        self.metadata_focus = None;
        match self.composer.handle_paste(text) {
            PromptEvent::Edited => {
                self.error = None;
                FeedbackModalOutcome::Changed
            }
            PromptEvent::Ignored => FeedbackModalOutcome::Changed,
        }
    }

    pub fn handle_mouse(&mut self, mouse: &MouseEvent) -> FeedbackModalOutcome {
        // Close, tab, and content clicks must not escape an in-flight or terminal draft send.
        if self.submit_pending || self.is_draft_submit_terminal() {
            return FeedbackModalOutcome::Changed;
        }
        // Only a click cancels an in-flight draft load. Mouse-move would drop the get
        // started when a draft-backed modal opens on Write.
        if self.active_tab() == FeedbackTab::Write
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.invalidate_draft_load();
        }
        if (self.draft_delete.is_some() || self.delete_confirm.is_some())
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            return FeedbackModalOutcome::Changed;
        }
        match modal_window::handle_modal_mouse(
            &mut self.window,
            mouse.kind,
            mouse.column,
            mouse.row,
        ) {
            ModalWindowOutcome::CloseRequested
            | ModalWindowOutcome::ShortcutActivated(CANCEL_SHORTCUT_ID) => {
                FeedbackModalOutcome::Cancel
            }
            ModalWindowOutcome::TabChanged(index) => {
                if FeedbackTab::from_index(index) == FeedbackTab::Drafts {
                    self.activate_drafts_tab();
                } else {
                    self.enter_write();
                }
                FeedbackModalOutcome::Changed
            }
            ModalWindowOutcome::Unhandled => {
                if self.active_tab() == FeedbackTab::Drafts
                    && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && self.discard_confirm.is_none()
                    && self.draft_load.is_none()
                {
                    self.handle_draft_click(mouse.column, mouse.row);
                    return FeedbackModalOutcome::Changed;
                }
                // An open picker owns the content area: clicks pick rows via its hit areas.
                if self.enum_picker.is_some() {
                    return self.handle_enum_picker_event(&crossterm::event::Event::Mouse(*mouse));
                }
                // Drafts, in-flight send/load, and the trace step hide or lock the composer.
                if !self.blocks_composer_input() {
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        // A click on a rendered label row focuses that field; the composer
                        // never sees it, so its cursor stays put.
                        if let Some(field) = self.metadata_field_at(mouse.column, mouse.row) {
                            self.metadata_focus = Some(field);
                            return FeedbackModalOutcome::Changed;
                        }
                        // Like paste, any other click targets the composer: it takes focus back
                        // from the label rows so the next Enter submits instead of editing a label.
                        self.metadata_focus = None;
                    }
                    self.composer.handle_mouse(mouse);
                }
                FeedbackModalOutcome::Changed
            }
            _ => FeedbackModalOutcome::Changed,
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
