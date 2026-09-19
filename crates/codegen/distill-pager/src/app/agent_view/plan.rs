// Modified for Distill by Samuel Fajreldines, 2026.
//! Plan UI: the plan chip and preview, plan approval and feedback, and casual plan commenting (incl. the casual-commenting test fixture).
use super::AgentView;
#[cfg(test)]
use super::{ActivePane, InputMode, test_fixtures};
#[cfg(test)]
use crate::actions::ActionRegistry;
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::scrollback::RenderBlock;
use crate::scrollback::blocks::SessionEvent;
use crate::views::file_search::line_viewer::LineViewerState;
use crate::views::list_pane::ListItem;
use crate::views::plan_approval_view::{
    PlanApprovalFocus, PlanApprovalViewState, PlanComment, PlanReviewOutcome, PlanReviewSource,
};
use crate::views::prompt_widget::{EnterOutcome, PromptEvent};
#[cfg(test)]
use crossterm::event::KeyModifiers;
use crossterm::event::{KeyCode, KeyEvent};
use std::io::Read;
pub(crate) const MAX_KEPT_PLAN_FILE_BYTES: u64 = crate::acp::MAX_PLAN_FILE_BYTES as u64;
/// Shared by post-turn revise / abandon while ExecutePlan is already in flight.
pub(crate) const BUILD_IN_FLIGHT_REVISE_NOTICE: &str =
    "Wait for the current turn to end before revising the plan.";
pub(crate) const BUILD_IN_FLIGHT_ABANDON_NOTICE: &str =
    "Wait for the current turn to end before abandoning the plan.";
pub(crate) const LEAVE_PLAN_REVISE_NOTICE: &str =
    "Wait for plan mode to finish switching before revising the plan.";
pub(crate) const PLAN_CHANGED_ON_DISK_NOTICE: &str =
    "The plan changed on disk. Review the updated plan before approving.";
pub(crate) fn capped_kept_plan_body(text: String) -> Option<String> {
    let len = u64::try_from(text.len()).ok()?;
    (len <= MAX_KEPT_PLAN_FILE_BYTES && !text.trim().is_empty()).then_some(text)
}
fn read_kept_plan_file(path: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut limited = file.take(MAX_KEPT_PLAN_FILE_BYTES.saturating_add(1));
    let mut body = String::new();
    limited.read_to_string(&mut body).ok()?;
    capped_kept_plan_body(body)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostTurnPlanCommit {
    Approved,
    Revised,
    Abandoned,
}
/// Telemetry for every way a plan review resolves ("build", "abandon", "revise").
fn log_plan_submit(action: &str) {
    use distill_telemetry::events::PlanSubmit;
    use distill_telemetry::session_ctx::log_event;
    log_event(PlanSubmit {
        action: action.to_string(),
    });
}
impl AgentView {
    /// Resolve the absolute path to the plan file for this session.
    fn plan_file_path(&self) -> Option<std::path::PathBuf> {
        let session_id = self.session.session_id.as_ref()?;
        let cwd_str = self.session.cwd.to_string_lossy().into_owned();
        let encoded_cwd = urlencoding::encode(&cwd_str);
        Some(
            distill_shell::util::distill_home::distill_home()
                .join("sessions")
                .join(encoded_cwd.as_ref())
                .join(session_id.0.as_ref())
                .join("plan.md"),
        )
    }
    /// Whether the current line viewer is showing a plan preview.
    pub(super) fn is_plan_viewer(&self) -> bool {
        self.line_viewer.as_ref().is_some_and(|v| {
            v.kind == crate::views::file_search::line_viewer::LineViewerKind::PlanPreview
        })
    }
    /// Whether the user is composing a comment via the prompt input inside the *casual* plan preview (the modal opened with no `plan_approval_view`).
    /// Mirrors the `pav.focus == Commenting` check used by the plan-approval path so the prompt/footer behaves identically across both modes.
    pub(super) fn is_casual_commenting(&self) -> bool {
        self.plan_approval_view.is_none()
            && self.is_plan_viewer()
            && self.casual_commenting_range.is_some()
    }
    /// Whether plan content is available for preview.
    /// Answers from mounted review / KeptPlan / path existence without reading the file.
    fn plan_preview_available(&self) -> bool {
        if self
            .plan_approval_view
            .as_ref()
            .and_then(|pav| pav.plan_content.as_deref())
            .is_some_and(|text| !text.trim().is_empty())
        {
            return true;
        }
        if self.kept_plan.preview_available() {
            return true;
        }
        self.plan_file_path().is_some_and(|path| path.is_file())
    }
    /// Whether the "plan" status-bar chip should be rendered.
    /// Visible while plan mode is active, or always when the user has set `show_plan_chip = true` in `pager.toml`.
    /// Hidden by default once the user exits plan mode.
    pub(super) fn should_show_plan_chip(
        &self,
        appearance: &crate::appearance::AppearanceConfig,
    ) -> bool {
        (self.plan_mode_active || appearance.show_plan_chip) && self.plan_preview_available()
    }
    /// Take the review and restore every UI field. Callers answer or cancel
    /// the returned view and, when needed, write the verdict row.
    pub(crate) fn unmount_plan_review(&mut self) -> Option<PlanApprovalViewState> {
        let mut pav = self.plan_approval_view.take()?;
        self.plan_next_comment_id = pav.next_comment_id;
        self.prompt.restore(std::mem::take(&mut pav.stashed_prompt));
        self.line_viewer = None;
        self.casual_commenting_range = None;
        self.casual_editing_comment_id = None;
        self.plan_freeform_prefill_deferred = false;
        Some(pav)
    }
    /// Close approve/build without a verdict row. Leaves the waiting plan so
    /// returning to Plan can reopen it. In-turn reviews stay: a mode change
    /// must not send_cancelled a held exit_plan_mode.
    pub(crate) fn dismiss_plan_review_ui(&mut self) {
        if self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.is_in_turn())
        {
            return;
        }
        if let Some(mut pav) = self.unmount_plan_review() {
            pav.send_stale_cancel();
        }
    }
    /// ExecutePlan has a prompt id and the post-turn review is still mounted.
    /// Abandon, revise, and Shift+Tab must toast and stay; a later refuse is
    /// not a successful build.
    pub(crate) fn is_post_turn_build_starting(&self) -> bool {
        self.execute_plan.is_some()
            && self
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.is_after_turn())
    }
    pub(crate) fn execute_plan_prompt_id(&self) -> Option<&str> {
        self.execute_plan.as_deref()
    }
    pub(crate) fn set_execute_plan_prompt(&mut self, prompt_id: impl Into<String>) {
        self.execute_plan = Some(prompt_id.into());
    }
    /// Stage a plan-mode flip and the keep that goes with it.
    /// Leave with a mounted after-turn review records Abandoned and stays
    /// until Default confirms, so a refused SetSessionMode does not commit.
    /// An unmounted keep is forgotten immediately. Leaving to Ask matches
    /// the worker: the keep is forgotten once the mode update lands.
    pub(crate) fn stage_plan_mode(&mut self, on: bool) {
        self.plan_mode_pending = Some(on);
        if on {
            self.pending_post_turn_commit = None;
            return;
        }
        if self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.is_after_turn())
        {
            self.pending_post_turn_commit = Some(PostTurnPlanCommit::Abandoned);
            return;
        }
        self.forget_waiting_plan();
    }
    /// Leave Plan after an approved build. No-op if Default already left, or
    /// if the user staged a later mode change (`plan_mode_pending` is set).
    /// Pending stays None so a later agent Plan entry is not user-requested.
    pub(crate) fn leave_plan_after_approved_build(&mut self) {
        if self.plan_mode_pending.is_some() || !self.plan_mode_active {
            return;
        }
        self.plan_mode_active = false;
        self.plan_mode_pending = None;
    }
    /// Append review notes onto planFileContent. The worker ignores prompt text
    /// on ExecutePlan and only reads `_meta.executePlan`.
    pub(crate) fn plan_content_with_review(content: String, notes: Option<&str>) -> String {
        let Some(notes) = notes.filter(|text| !text.trim().is_empty()) else {
            return content;
        };
        if content.contains(notes) {
            return content;
        }
        if content.trim().is_empty() {
            notes.to_owned()
        } else {
            format!("{}\n\n{notes}", content.trim_end())
        }
    }
    /// Forget a waiting CreatePlan. Leaves `execute_plan_prompt_id` so a later
    /// `PromptResponse` can still match the dispatched build.
    /// Also drops approve/build: Default confirm has already cleared `last_plan`.
    pub(crate) fn forget_waiting_plan(&mut self) {
        self.kept_plan.clear();
        self.pending_post_turn_commit = None;
        self.dismiss_plan_review_ui();
    }
    /// Approve and abandon forget the waiting plan. Revise must not call this.
    /// Post-turn approve waits until Default is confirmed (the build started).
    pub(crate) fn clear_kept_plan(&mut self) {
        self.forget_waiting_plan();
        self.execute_plan = None;
    }
    /// `true` when this `PromptResponse` is the post-turn build that approve dispatched.
    pub(crate) fn take_execute_plan_prompt(&mut self, prompt_id: Option<&str>) -> bool {
        match (self.execute_plan_prompt_id(), prompt_id) {
            (Some(expected), Some(got)) if expected == got => {
                self.drop_execute_plan_prompt();
                true
            }
            _ => false,
        }
    }
    /// Drop a build id whose `session/prompt` died with the ACP channel.
    /// Keep it only when session/load adopts that same turn.
    pub(crate) fn release_stale_execute_plan_prompt(&mut self, running_prompt_id: Option<&str>) {
        let keep = running_prompt_id.is_some_and(|pid| {
            self.execute_plan_prompt_id() == Some(pid) && self.should_adopt_running_prompt(pid)
        });
        if !keep {
            self.drop_execute_plan_prompt();
        }
    }
    fn drop_execute_plan_prompt(&mut self) {
        self.execute_plan = None;
        if matches!(
            self.pending_post_turn_commit,
            Some(PostTurnPlanCommit::Approved)
        ) {
            self.pending_post_turn_commit = None;
        }
    }
    fn inline_plan_content(&self) -> Option<&str> {
        self.plan_approval_view
            .as_ref()
            .filter(|p| p.source == PlanReviewSource::Inline)
            .and_then(|p| p.plan_content.as_deref())
            .filter(|s| !s.trim().is_empty())
    }
    /// Resolve the plan body for the line-viewer preview.
    /// Prefers content carried on the approval request (inline plan-creation or the shell-read file body), then falls back to the on-disk plan file.
    /// Request body first keeps file-backed previews working when the path resolution fails or the file disappears between intercept and open.
    pub(super) fn plan_body_for_preview(&self) -> Option<String> {
        if let Some(content) = self
            .plan_approval_view
            .as_ref()
            .and_then(|p| p.plan_content.as_deref())
            .filter(|s| !s.trim().is_empty())
        {
            return Some(content.to_owned());
        }
        if let Some(content) = self.kept_plan.review_content(read_kept_plan_file) {
            return Some(content);
        }
        self.plan_file_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .filter(|s| !s.trim().is_empty())
    }
    /// An in-turn review's ext method dies with the turn. A post-turn review stays until the user decides.
    pub(crate) fn dismiss_in_turn_plan_review(&mut self) -> bool {
        if self
            .plan_approval_view
            .as_ref()
            .is_none_or(|pav| pav.is_after_turn())
        {
            return false;
        }
        if let Some(mut pav) = self.unmount_plan_review() {
            pav.send_stale_cancel();
        }
        true
    }
    /// EndTurn / fail-reopen. Stay closed while leave-Plan is pending or a
    /// build is already in flight (`execute_plan_prompt_id`): re-entering Plan
    /// must not remount approve/build or let PromptResponse write a second row.
    pub(crate) fn open_post_turn_plan_review(&mut self) {
        if self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.is_after_turn())
        {
            self.refresh_post_turn_plan_review();
            return;
        }
        if !self.post_turn_plan_review
            || self.plan_approval_view.is_some()
            || !self.plan_mode_pending.unwrap_or(self.plan_mode_active)
            || !self.kept_plan.is_kept()
            || self.execute_plan.is_some()
        {
            return;
        }
        let Some(content) = self.post_turn_review_plan_content() else {
            return;
        };
        let stashed = self.prompt.stash();
        self.plan_approval_view = Some(PlanApprovalViewState::after_turn(
            "CreatePlan".to_owned(),
            content,
            stashed,
        ));
        self.show_plan_preview_if_available();
    }
    /// Queued follow-up CreatePlan can replace the keep while approve
    /// is still mounted. Refresh the snapshot; do not remount.
    pub(crate) fn refresh_post_turn_plan_review(&mut self) {
        if self.execute_plan.is_some()
            || self
                .plan_approval_view
                .as_ref()
                .is_none_or(|pav| pav.is_in_turn())
        {
            return;
        }
        let Some(content) = self.post_turn_review_plan_content() else {
            return;
        };
        {
            let Some(pav) = self.plan_approval_view.as_mut() else {
                return;
            };
            if pav.plan_content.as_deref() == Some(content.as_str()) {
                return;
            }
            pav.plan_content = Some(content);
            pav.has_plan = true;
        }
        self.show_plan_preview_if_available();
    }
    fn post_turn_review_plan_content(&self) -> Option<String> {
        self.kept_plan.review_content(read_kept_plan_file)
    }
    /// Open the plan preview when content exists, or when plan approval is parked with an empty body (so the decision surface always pops).
    pub(crate) fn show_plan_preview_if_available(&mut self) {
        if self.plan_preview_available() || self.plan_approval_view.is_some() {
            self.show_plan_preview();
        }
    }
    /// Show the plan in the line viewer overlay or a "no plan" toast.
    /// When plan approval is parked without a body, opens a placeholder preview.
    /// The user then always sees a decision surface (a/s/q) instead of a dead "Waiting on plan approval" line with a no-op Tab:plan.
    pub fn show_plan_preview(&mut self) {
        let body = self.plan_body_for_preview();
        let approval_empty = self
            .plan_approval_view
            .as_ref()
            .is_some_and(|p| !p.has_plan);
        let Some(mut viewer) = (if let Some(content) = body {
            LineViewerState::open_markdown_content("plan.md", content, None)
        } else if approval_empty {
            LineViewerState::open_markdown_content(
                "plan.md",
                crate::views::plan_approval_view::EMPTY_PLAN_PLACEHOLDER.to_owned(),
                None,
            )
        } else if let Some(plan_path) = self.plan_file_path() {
            LineViewerState::open_markdown(&plan_path, None)
        } else {
            None
        }) else {
            self.show_toast("No plan written yet.");
            return;
        };
        viewer.kind = crate::views::file_search::line_viewer::LineViewerKind::PlanPreview;
        viewer.title_override = Some(if approval_empty {
            "plan.md (empty)".to_string()
        } else {
            "plan.md".to_string()
        });
        viewer.fullscreen = true;
        {
            let plan = viewer.plan_mut();
            plan.show_action_buttons = self.plan_approval_view.is_none();
            plan.feedback_active = self.plan_approval_view.is_some();
        }
        if let Some(ref pav) = self.plan_approval_view
            && !pav.comments.is_empty()
        {
            viewer.rebuild_with_comments(&pav.comments);
        } else if !self.plan_comments.is_empty() {
            viewer.rebuild_with_comments(&self.plan_comments);
        }
        self.line_viewer = Some(viewer);
    }
    /// Test fixture: drive the agent into casual-commenting state (line viewer open in plan-preview mode, `casual_commenting_range` set).
    /// Makes the `Event::Paste` plan-feedback arm reachable from a unit test without spawning the real keystroke pipeline.
    /// One helper instead of three field mutations, so a refactor of this state only updates the fixture.
    #[cfg(test)]
    pub(crate) fn enter_casual_commenting_for_test(&mut self) {
        let mut viewer =
            crate::views::file_search::line_viewer::LineViewerState::open_markdown_content(
                "test.md",
                "hello\n".to_owned(),
                None,
            )
            .expect("fixture must open the line viewer");
        viewer.kind = crate::views::file_search::line_viewer::LineViewerKind::PlanPreview;
        self.line_viewer = Some(viewer);
        self.casual_commenting_range = Some(0..1);
    }
    /// Same gate the queued-row editor applies before Enter (`queue_edit.rs`), so dispatch parity holds:
    /// raw text, registry-known, dispatchable, args complete.
    fn is_freeform_builtin_slash_command(&self) -> bool {
        crate::slash::is_complete_builtin_invocation(
            self.prompt.text(),
            self.prompt.slash_controller.registry(),
        )
    }
    pub(crate) fn approve_plan(&mut self) -> InputOutcome {
        if self.is_freeform_builtin_slash_command()
            && let Some(pav) = self.plan_approval_view.as_mut()
        {
            let msg = match pav.focus {
                PlanApprovalFocus::Commenting => {
                    "The comment in progress is a slash command: finish or discard it before approving."
                }
                PlanApprovalFocus::Preview | PlanApprovalFocus::Prompt => {
                    pav.focus = PlanApprovalFocus::Prompt;
                    "Run the slash command in the notes with Enter, or clear it, before approving."
                }
            };
            if crate::app::minimal_mode_active() {
                self.scrollback.push_block(RenderBlock::system(msg));
            } else {
                self.show_toast(msg);
            }
            return InputOutcome::Changed;
        }
        let post_turn = self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.is_after_turn());
        let live_keep = post_turn
            .then(|| self.post_turn_review_plan_content())
            .flatten();
        let Some(pav) = self.plan_approval_view.as_ref() else {
            return InputOutcome::Changed;
        };
        let freeform = {
            let t = self.prompt.text_without_image_chips();
            let trimmed = t.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        };
        let review_comments = {
            let formatted = pav.format_feedback(freeform.as_deref());
            if formatted.trim().is_empty() {
                None
            } else {
                Some(format!(
                    "The user approved the plan with the following review comments:\n\n{}",
                    formatted
                ))
            }
        };
        if pav.is_after_turn() {
            let snapshot = pav.plan_content.clone().unwrap_or_default();
            if let Some(disk) = live_keep
                && disk != snapshot
            {
                if let Some(pav) = self.plan_approval_view.as_mut() {
                    pav.plan_content = Some(disk);
                    pav.has_plan = true;
                }
                self.show_plan_preview_if_available();
                self.show_toast(PLAN_CHANGED_ON_DISK_NOTICE);
                return InputOutcome::Changed;
            }
            let notes = review_comments.as_deref();
            return InputOutcome::Action(Action::ExecutePlan {
                plan_file_content: Self::plan_content_with_review(snapshot, notes),
                plan_file_uri: notes
                    .filter(|text| !text.trim().is_empty())
                    .is_none()
                    .then(|| self.kept_plan.file_uri())
                    .flatten(),
            });
        }
        if let Some(pav) = self.plan_approval_view.as_mut() {
            Self::merge_live_images_into_stash(&mut self.prompt, &mut pav.stashed_prompt);
        }
        let Some(mut pav) = self.unmount_plan_review() else {
            return InputOutcome::Changed;
        };
        pav.send_approved();
        self.close_plan_review_and_forget(PlanReviewOutcome::Approved);
        if let Some(text) = review_comments {
            return InputOutcome::Action(Action::Interject {
                text,
                images: vec![],
            });
        }
        InputOutcome::Changed
    }
    /// Fold freeform-only images into the session draft.
    /// Prefill clones share `display_number` *and* payload with the session image and are dropped.
    /// Number reuse after freeform clear (Ctrl+C resets the counter) is not a clone and must renumber-merge.
    fn merge_live_images_into_stash(
        prompt: &mut crate::views::prompt_widget::PromptWidget,
        session: &mut crate::views::prompt_widget::StashedPrompt,
    ) {
        let live = prompt.drain_images();
        for mut img in live {
            if session.images.iter().any(|s| {
                s.display_number == img.display_number && Self::same_image_payload(s, &img)
            }) {
                crate::prompt_images::cleanup_image(
                    crate::prompt_images::SessionPathPolicy::Preserve,
                    &img,
                );
                continue;
            }
            session.image_counter = session.image_counter.max(
                session
                    .images
                    .iter()
                    .map(|i| i.display_number)
                    .max()
                    .unwrap_or(0),
            );
            session.image_counter += 1;
            let dn = session.image_counter;
            img.display_number = dn;
            if !session.text.is_empty()
                && !session.text.ends_with(' ')
                && !session.text.ends_with('\n')
            {
                session.text.push(' ');
            }
            let placeholder = crate::prompt_images::display_text(dn);
            let start = session.text.len();
            session.text.push_str(&placeholder);
            let end = session.text.len();
            session.text.push(' ');
            session.chip_elements.push(crate::app::agent::ChipElement {
                range: start..end,
                kind: crate::views::prompt_widget::KIND_IMAGE,
                display: None,
            });
            session.images.push(img);
            session.cursor = session.text.len();
        }
    }
    /// Content identity for prefill-clone detection (not display_number alone).
    fn same_image_payload(
        a: &crate::prompt_images::PastedImage,
        b: &crate::prompt_images::PastedImage,
    ) -> bool {
        match (&a.encoded_bytes, &b.encoded_bytes) {
            (Some(ea), Some(eb)) if ea == eb => return true,
            _ => {}
        }
        match (&a.session_image_path, &b.session_image_path) {
            (Some(pa), Some(pb)) if pa == pb => return true,
            _ => {}
        }
        match (&a.source_path, &b.source_path) {
            (Some(pa), Some(pb)) if pa == pb => return true,
            _ => {}
        }
        false
    }
    pub(crate) fn abandon_plan(&mut self) -> InputOutcome {
        let Some(pav) = self.plan_approval_view.as_ref() else {
            return InputOutcome::Changed;
        };
        if self.is_post_turn_build_starting() {
            self.show_toast("Wait for the current turn to end before abandoning the plan.");
            return InputOutcome::Changed;
        }
        if pav.is_after_turn() {
            return InputOutcome::Action(Action::SetPlanMode(
                crate::app::actions::PlanModeKind::Off,
            ));
        }
        let Some(mut pav) = self.unmount_plan_review() else {
            return InputOutcome::Changed;
        };
        pav.send_abandoned();
        self.close_plan_review_and_forget(PlanReviewOutcome::Abandoned);
        InputOutcome::Changed
    }
    /// The shell leaves plan mode, but its confirming `CurrentModeUpdate("default")` is fire-and-forget and only arrives after the exit tool runs.
    /// So flip the mode indicator optimistically here; a lost update would otherwise leave the badge stuck on "plan".
    /// Not for the revision path (`send_plan_feedback`): the shell stays in plan mode there, so the indicator must stay on.
    /// Post-turn approve / abandon commit from `commit_post_turn_plan_*` after
    /// ExecutePlan / SetPlanMode(Off) is accepted.
    fn close_plan_review_and_forget(&mut self, outcome: PlanReviewOutcome) {
        if self.plan_mode_pending.unwrap_or(self.plan_mode_active) {
            self.plan_mode_pending = Some(false);
        }
        self.clear_kept_plan();
        self.finish_plan_review_ui(outcome);
    }
    fn finish_plan_review_ui(&mut self, outcome: PlanReviewOutcome) {
        self.scrollback
            .push_block(RenderBlock::session_event(SessionEvent::PlanReviewClosed {
                outcome,
                permission: self.session.permission_label(),
            }));
        log_plan_submit(match outcome {
            PlanReviewOutcome::Approved => "build",
            PlanReviewOutcome::Abandoned => "abandon",
        });
    }
    /// Close a post-turn review only after its follow-up dispatch is accepted.
    /// Approve, revise, and abandon share this so a refuse cannot record a
    /// verdict, toast "sent", drop comments, or skip session/set_mode.
    pub(crate) fn commit_post_turn_plan_approved(&mut self) {
        self.commit_post_turn_plan_review(PostTurnPlanCommit::Approved);
    }
    pub(crate) fn commit_post_turn_plan_revised(&mut self) {
        self.commit_post_turn_plan_review(PostTurnPlanCommit::Revised);
    }
    pub(crate) fn commit_post_turn_plan_abandoned(&mut self) {
        self.commit_post_turn_plan_review(PostTurnPlanCommit::Abandoned);
    }
    pub(crate) fn commit_post_turn_plan_review(&mut self, commit: PostTurnPlanCommit) {
        if self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.is_in_turn())
        {
            return;
        }
        if let Some(pav) = self.plan_approval_view.as_mut() {
            Self::merge_live_images_into_stash(&mut self.prompt, &mut pav.stashed_prompt);
        }
        let Some(mut pav) = self.unmount_plan_review() else {
            return;
        };
        self.pending_post_turn_commit = None;
        match commit {
            PostTurnPlanCommit::Approved => {
                pav.send_approved();
                self.finish_plan_review_ui(PlanReviewOutcome::Approved);
                self.leave_plan_after_approved_build();
            }
            PostTurnPlanCommit::Abandoned => {
                pav.send_abandoned();
                self.close_plan_review_and_forget(PlanReviewOutcome::Abandoned);
            }
            PostTurnPlanCommit::Revised => {
                pav.send_cancelled(None);
                self.kept_plan.drop_body_if_pathed();
                self.prompt.textarea.cancel_undo_group();
                self.show_toast("Plan revision sent.");
                log_plan_submit("revise");
            }
        }
    }
    fn send_plan_feedback(&mut self, feedback: Option<String>) -> InputOutcome {
        let Some(pav) = self.plan_approval_view.as_ref() else {
            return InputOutcome::Changed;
        };
        let formatted = pav.format_feedback(feedback.as_deref());
        let to_send = if formatted.trim().is_empty() {
            feedback
        } else {
            Some(formatted)
        };
        let post_turn = pav.is_after_turn();
        if !post_turn
            && self.is_minimal_mode()
            && let Some(msg) = to_send.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            self.scrollback
                .push_block(crate::scrollback::RenderBlock::user_prompt(msg.to_string()));
        }
        if post_turn && to_send.as_deref().is_none_or(|text| text.trim().is_empty()) {
            self.show_toast("Type revision notes, or press a to approve.");
            return InputOutcome::Changed;
        }
        if self.is_post_turn_build_starting() {
            self.show_toast(BUILD_IN_FLIGHT_REVISE_NOTICE);
            return InputOutcome::Changed;
        }
        if post_turn && self.plan_mode_pending == Some(false) {
            self.show_toast(LEAVE_PLAN_REVISE_NOTICE);
            return InputOutcome::Changed;
        }
        if post_turn {
            let text = to_send.unwrap_or_default();
            return InputOutcome::Action(Action::RevisePlan(text));
        }
        if let Some(pav) = self.plan_approval_view.as_mut() {
            Self::merge_live_images_into_stash(&mut self.prompt, &mut pav.stashed_prompt);
        }
        let Some(mut pav) = self.unmount_plan_review() else {
            return InputOutcome::Changed;
        };
        pav.send_cancelled(to_send.clone());
        if pav.source == PlanReviewSource::Inline {
            self.kept_plan.clear_body();
        }
        self.prompt.textarea.cancel_undo_group();
        self.show_toast("Plan revision sent.");
        log_plan_submit("revise");
        InputOutcome::Changed
    }
    pub(crate) fn reopen_plan_approval(&mut self) {
        if let Some(ref mut pav) = self.plan_approval_view {
            pav.focus = PlanApprovalFocus::Preview;
        }
        self.show_plan_preview_if_available();
        if self.line_viewer.is_none() {
            if let Some(ref mut pav) = self.plan_approval_view {
                pav.focus = PlanApprovalFocus::Prompt;
            }
        } else if let Some(ref mut viewer) = self.line_viewer {
            viewer.plan_mut().feedback_active = true;
        }
    }
    fn leave_plan_commenting_restore_freeform(&mut self) {
        let stashed = if let Some(ref mut pav) = self.plan_approval_view {
            pav.commenting_range = None;
            pav.editing_comment_id = None;
            pav.stashed_feedback_prompt.take()
        } else {
            None
        };
        if let Some(stashed) = stashed {
            self.prompt.restore(stashed);
        } else {
            self.prompt.set_text("");
        }
    }
    pub(super) fn discard_in_progress_comment(&mut self) {
        self.leave_plan_commenting_restore_freeform();
    }
    pub(super) fn handle_plan_feedback_key(&mut self, key: &KeyEvent) -> InputOutcome {
        let is_commenting = self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.focus == PlanApprovalFocus::Commenting);
        if crate::input::key::RowWalk::from_key(key).is_some() {
            let focus = self.plan_approval_view.as_ref().map(|p| p.focus);
            match focus {
                Some(PlanApprovalFocus::Prompt) | Some(PlanApprovalFocus::Commenting) => {
                    if self.line_viewer.is_none() {
                        self.show_plan_preview_if_available();
                    }
                    if let Some(ref mut pav) = self.plan_approval_view {
                        pav.focus = PlanApprovalFocus::Preview;
                    }
                    if let Some(ref mut viewer) = self.line_viewer {
                        viewer.plan_mut().feedback_active = true;
                    }
                }
                Some(PlanApprovalFocus::Preview) => {
                    if let Some(ref mut pav) = self.plan_approval_view {
                        pav.focus = PlanApprovalFocus::Prompt;
                    }
                }
                None => {}
            }
            if is_commenting {
                self.discard_in_progress_comment();
            }
            return InputOutcome::Changed;
        }
        if key.code == KeyCode::Esc {
            if self.prompt.file_search_visible() {
                self.prompt.file_search.clear_context();
                return InputOutcome::Changed;
            }
            if is_commenting {
                if let Some(ref mut pav) = self.plan_approval_view {
                    pav.focus = PlanApprovalFocus::Preview;
                }
                self.discard_in_progress_comment();
                return InputOutcome::Changed;
            }
            if let Some(ref mut pav) = self.plan_approval_view {
                pav.focus = PlanApprovalFocus::Preview;
            }
            return InputOutcome::Changed;
        }
        if !is_commenting
            && key.code == KeyCode::Char('a')
            && key.modifiers.is_empty()
            && self.prompt.text_without_image_chips().trim().is_empty()
            && !self.prompt.file_search_visible()
        {
            return self.approve_plan();
        }
        match self.prompt.route_enter(key) {
            EnterOutcome::NewlineInserted => return InputOutcome::Changed,
            EnterOutcome::Submit => {
                if is_commenting {
                    return self.save_plan_comment();
                }
                let freeform_text = self.prompt.text_without_image_chips();
                let has_comments = self
                    .plan_approval_view
                    .as_ref()
                    .is_some_and(|pav| !pav.comments.is_empty());
                let prompt_focused = self
                    .plan_approval_view
                    .as_ref()
                    .is_some_and(|pav| pav.focus == PlanApprovalFocus::Prompt);
                if prompt_focused {
                    if self.is_freeform_builtin_slash_command() {
                        let text = self.prompt.text().to_owned();
                        if let Some(pav) = self.plan_approval_view.as_mut() {
                            let consumed_text = pav.stashed_prompt.text.trim() == text.trim();
                            let consumed_images = self.prompt.images.iter().any(|live| {
                                pav.stashed_prompt
                                    .images
                                    .iter()
                                    .any(|stashed| Self::same_image_payload(stashed, live))
                            });
                            if consumed_text || consumed_images {
                                pav.stashed_prompt =
                                    crate::views::prompt_widget::StashedPrompt::default();
                            }
                        }
                        return InputOutcome::Action(Action::SendPrompt(text));
                    }
                    if freeform_text.trim().is_empty() && !has_comments {
                        self.show_toast("Type revision notes, or press a to approve.");
                        return InputOutcome::Changed;
                    }
                    let freeform = {
                        let trimmed = freeform_text.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_owned())
                        }
                    };
                    return self.send_plan_feedback(freeform);
                }
                return InputOutcome::Changed;
            }
            EnterOutcome::PassThrough => {}
        }
        match self.prompt.handle_key(key) {
            PromptEvent::Edited => {
                if let Some(req) = self.prompt.pending_viewer_request.take() {
                    self.open_line_viewer(&req.path, req.initial_range);
                }
                InputOutcome::Changed
            }
            PromptEvent::Ignored => InputOutcome::Changed,
        }
    }
    pub(super) fn enter_plan_commenting(&mut self) -> InputOutcome {
        let viewer = match self.line_viewer.as_mut() {
            Some(v) => v,
            None => return InputOutcome::Changed,
        };
        if let Some(vi) = viewer.list_state.selected_index() {
            let pi = viewer.list_state.to_physical(vi);
            if let Some(comment_id) = viewer.lines.get(pi).and_then(|item| item.comment_id())
                && let Some(pav) = self.plan_approval_view.as_mut()
                && let Some(comment) = pav.comments.iter().find(|c| c.id == comment_id)
            {
                let comment_text = comment.text.clone();
                let comment_range = comment.line_range.clone();
                if pav.stashed_feedback_prompt.is_none() {
                    pav.stashed_feedback_prompt = Some(self.prompt.stash());
                }
                pav.editing_comment_id = Some(comment_id);
                pav.commenting_range = Some(comment_range);
                pav.focus = PlanApprovalFocus::Commenting;
                self.prompt.set_text(&comment_text);
                return InputOutcome::Changed;
            }
        }
        let range = viewer.selected_line_range();
        let Some(range) = range else {
            return InputOutcome::Changed;
        };
        if viewer.list_state.visual_mode {
            let start_vi = viewer.list_state.multi_range().map(|r| r.start);
            if let Some(start_vi) = start_vi {
                let start_pi = viewer.list_state.to_physical(start_vi);
                let start_id = viewer.lines.get(start_pi).map(|l| l.stable_id());
                viewer.list_state.exit_visual_mode();
                if let Some(id) = start_id {
                    viewer.list_state.select_by_id(id);
                }
            } else {
                viewer.list_state.exit_visual_mode();
            }
        }
        if let Some(ref mut pav) = self.plan_approval_view {
            if pav.stashed_feedback_prompt.is_none() {
                pav.stashed_feedback_prompt = Some(self.prompt.stash());
            }
            pav.commenting_range = Some(range);
            pav.editing_comment_id = None;
            pav.focus = PlanApprovalFocus::Commenting;
        }
        self.prompt.set_text("");
        InputOutcome::Changed
    }
    fn save_plan_comment(&mut self) -> InputOutcome {
        let text = self.prompt.text().to_string();
        if text.trim().is_empty() {
            return InputOutcome::Changed;
        }
        let pav = match self.plan_approval_view.as_mut() {
            Some(pav) => pav,
            None => return InputOutcome::Changed,
        };
        let range = match pav.commenting_range.take() {
            Some(r) => r,
            None => return InputOutcome::Changed,
        };
        if let Some(edit_id) = pav.editing_comment_id.take() {
            if let Some(comment) = pav.comments.iter_mut().find(|c| c.id == edit_id) {
                comment.text = text;
                comment.line_range = range;
            }
        } else {
            let id = pav.next_comment_id;
            pav.next_comment_id += 1;
            pav.comments.push(PlanComment {
                id,
                line_range: range,
                text,
            });
        }
        pav.focus = PlanApprovalFocus::Preview;
        let comments = pav.comments.clone();
        if let Some(ref mut viewer) = self.line_viewer {
            viewer.rebuild_with_comments(&comments);
        }
        if let Some(stashed) = pav.stashed_feedback_prompt.take() {
            self.prompt.restore(stashed);
        } else {
            self.prompt.set_text("");
        }
        InputOutcome::Changed
    }
    pub(super) fn delete_plan_comment_at_cursor(&mut self) -> InputOutcome {
        let viewer = match self.line_viewer.as_ref() {
            Some(v) => v,
            None => return InputOutcome::Changed,
        };
        let vi = match viewer.list_state.selected_index() {
            Some(vi) => vi,
            None => return InputOutcome::Changed,
        };
        let pi = viewer.list_state.to_physical(vi);
        let comment_id = match viewer.lines.get(pi).and_then(|item| item.comment_id()) {
            Some(id) => id,
            None => return InputOutcome::Changed,
        };
        if let Some(ref mut pav) = self.plan_approval_view {
            pav.comments.retain(|c| c.id != comment_id);
            let comments = pav.comments.clone();
            if let Some(ref mut viewer) = self.line_viewer {
                viewer.rebuild_with_comments(&comments);
            }
        }
        InputOutcome::Changed
    }
    /// Enter casual commenting mode from the plan preview.
    /// If the cursor is on a comment line, enter edit mode for that comment.
    /// If the cursor is on a source line, capture the line range and enter new-comment mode.
    pub(super) fn enter_casual_plan_commenting(&mut self) -> InputOutcome {
        let viewer = match self.line_viewer.as_mut() {
            Some(v) => v,
            None => return InputOutcome::Changed,
        };
        if let Some(vi) = viewer.list_state.selected_index() {
            let pi = viewer.list_state.to_physical(vi);
            if let Some(comment_id) = viewer.lines.get(pi).and_then(|item| item.comment_id())
                && let Some(comment) = self.plan_comments.iter().find(|c| c.id == comment_id)
            {
                let comment_text = comment.text.clone();
                let comment_range = comment.line_range.clone();
                if self.casual_stashed_prompt.is_none() {
                    self.casual_stashed_prompt = Some(self.prompt.stash());
                }
                self.casual_editing_comment_id = Some(comment_id);
                self.casual_commenting_range = Some(comment_range);
                self.prompt.set_text(&comment_text);
                return InputOutcome::Changed;
            }
        }
        let range = viewer.selected_line_range();
        let Some(range) = range else {
            return InputOutcome::Changed;
        };
        if viewer.list_state.visual_mode {
            let start_vi = viewer.list_state.multi_range().map(|r| r.start);
            if let Some(start_vi) = start_vi {
                let start_pi = viewer.list_state.to_physical(start_vi);
                let start_id = viewer.lines.get(start_pi).map(|l| l.stable_id());
                viewer.list_state.exit_visual_mode();
                if let Some(id) = start_id {
                    viewer.list_state.select_by_id(id);
                }
            } else {
                viewer.list_state.exit_visual_mode();
            }
        }
        if self.casual_stashed_prompt.is_none() {
            self.casual_stashed_prompt = Some(self.prompt.stash());
        }
        self.casual_commenting_range = Some(range);
        self.casual_editing_comment_id = None;
        self.prompt.set_text("");
        InputOutcome::Changed
    }
    /// Save the current casual comment (new or edited) and rebuild the viewer.
    pub(super) fn save_casual_plan_comment(&mut self) -> InputOutcome {
        let text = self.prompt.text().to_owned();
        if text.trim().is_empty() {
            return self.cancel_casual_plan_commenting();
        }
        let range = match self.casual_commenting_range.take() {
            Some(r) => r,
            None => return self.cancel_casual_plan_commenting(),
        };
        if let Some(edit_id) = self.casual_editing_comment_id.take() {
            if let Some(comment) = self.plan_comments.iter_mut().find(|c| c.id == edit_id) {
                comment.text = text;
                comment.line_range = range;
            }
        } else {
            let id = self.plan_next_comment_id;
            self.plan_next_comment_id += 1;
            self.plan_comments.push(PlanComment {
                id,
                line_range: range,
                text,
            });
        }
        if let Some(stashed) = self.casual_stashed_prompt.take() {
            self.prompt.restore(stashed);
        } else {
            self.prompt.set_text("");
        }
        let comments = self.plan_comments.clone();
        if let Some(ref mut viewer) = self.line_viewer {
            viewer.rebuild_with_comments(&comments);
        }
        InputOutcome::Changed
    }
    /// Cancel casual plan commenting without saving.
    pub(super) fn cancel_casual_plan_commenting(&mut self) -> InputOutcome {
        self.casual_commenting_range = None;
        self.casual_editing_comment_id = None;
        if let Some(stashed) = self.casual_stashed_prompt.take() {
            self.prompt.restore(stashed);
        } else {
            self.prompt.set_text("");
        }
        InputOutcome::Changed
    }
    /// Key handler used while the user is composing a casual plan comment via the prompt input.
    /// Mirrors `handle_plan_feedback_key` (which serves the plan-approval Commenting focus) so the UX is identical.
    /// Enter saves, Esc cancels, Tab cancels back to the modal, and everything else routes to the prompt textarea.
    pub(super) fn handle_casual_plan_feedback_key(&mut self, key: &KeyEvent) -> InputOutcome {
        if key.code == KeyCode::Esc {
            if self.prompt.file_search_visible() {
                self.prompt.file_search.clear_context();
                return InputOutcome::Changed;
            }
            return self.cancel_casual_plan_commenting();
        }
        match self.prompt.route_enter(key) {
            EnterOutcome::NewlineInserted => return InputOutcome::Changed,
            EnterOutcome::Submit => return self.save_casual_plan_comment(),
            EnterOutcome::PassThrough => {}
        }
        if key.code == KeyCode::Tab && key.modifiers.is_empty() {
            return self.cancel_casual_plan_commenting();
        }
        match self.prompt.handle_key(key) {
            PromptEvent::Edited => {
                if let Some(req) = self.prompt.pending_viewer_request.take() {
                    self.open_line_viewer(&req.path, req.initial_range);
                }
                InputOutcome::Changed
            }
            PromptEvent::Ignored => InputOutcome::Changed,
        }
    }
    /// Delete the casual comment under the cursor in the plan preview.
    pub(super) fn delete_casual_plan_comment_at_cursor(&mut self) -> InputOutcome {
        let viewer = match self.line_viewer.as_ref() {
            Some(v) => v,
            None => return InputOutcome::Unchanged,
        };
        let vi = match viewer.list_state.selected_index() {
            Some(vi) => vi,
            None => return InputOutcome::Unchanged,
        };
        let pi = viewer.list_state.to_physical(vi);
        let comment_id = match viewer.lines.get(pi).and_then(|item| item.comment_id()) {
            Some(id) => id,
            None => return InputOutcome::Unchanged,
        };
        self.plan_comments.retain(|c| c.id != comment_id);
        let comments = self.plan_comments.clone();
        if let Some(ref mut viewer) = self.line_viewer {
            viewer.rebuild_with_comments(&comments);
        }
        InputOutcome::Changed
    }
    pub(super) fn send_casual_plan_comments(&mut self) -> InputOutcome {
        if self.plan_comments.is_empty() {
            self.show_toast("No comments to send.");
            return InputOutcome::Changed;
        }
        let plan_content = self.inline_plan_content().map(str::to_owned).or_else(|| {
            let path = self.plan_file_path()?;
            std::fs::read_to_string(path).ok()
        });
        let body = crate::views::plan_approval_view::format_plan_comments(
            &self.plan_comments,
            plan_content.as_deref(),
        );
        let text = format!("Plan feedback:\n\n{body}");
        self.plan_comments.clear();
        self.plan_next_comment_id = 0;
        self.cancel_line_viewer();
        self.show_toast("Plan feedback sent.");
        InputOutcome::Action(Action::SendPrompt(text))
    }
}
#[cfg(test)]
mod plan_chip_tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::agent::{AgentId, AgentSession, AgentState};
    use crate::appearance::AppearanceConfig;
    use crate::scrollback::state::ScrollbackState;
    fn make_agent() -> AgentView {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut agent = AgentView::new(
            AgentSession {
                id: AgentId(0),
                acp_tx: tx,
                session_id: None,
                models: ModelState::default(),
                state: AgentState::Idle,
                tracker: crate::acp::tracker::AcpUpdateTracker::new(),
                cwd: std::path::PathBuf::from("/tmp"),
                is_worktree: false,
                forked_from: None,
                pending_prompts: std::collections::VecDeque::new(),
                next_queue_id: 0,
                yolo_mode: false,
                auto_mode: false,
                prompt_history: Vec::new(),
                prompt_history_loading: false,
                loading_replay: false,
                restore_degree: None,
                rate_limited: false,
                model_incompatible: false,
                credit_limit_blocked: false,
                free_usage_blocked: false,
                available_commands: Vec::new(),
                available_commands_generation: 0,
                available_tools: None,
                model_switch_pending: false,
                hook_block_hold: false,
                blocked_prompt: None,
                user_model_preference: None,
                deferred_model_switch: None,
                bg_tasks: std::collections::BTreeMap::new(),
                bg_tool_call_to_task: std::collections::HashMap::new(),
                scheduled_tasks: std::collections::HashMap::new(),
                in_flight_prompt: None,
                compact_held_prompt: None,
                current_prompt_id: None,
                created_via_new: false,
            },
            ScrollbackState::new(),
        );
        agent.post_turn_plan_review = true;
        agent
    }
    #[test]
    fn plan_chip_hidden_after_exit_by_default() {
        let mut agent = make_agent();
        agent.plan_mode_active = false;
        let appearance = AppearanceConfig::default();
        assert!(!appearance.show_plan_chip);
        assert!(!agent.should_show_plan_chip(&appearance));
    }
    #[test]
    fn plan_chip_visible_while_plan_mode_active() {
        let mut agent = make_agent();
        agent.plan_mode_active = true;
        let appearance = AppearanceConfig::default();
        assert!(!agent.should_show_plan_chip(&appearance));
    }
    #[test]
    fn plan_chip_visible_when_config_overrides() {
        let mut agent = make_agent();
        agent.plan_mode_active = false;
        let appearance = AppearanceConfig {
            show_plan_chip: true,
            ..Default::default()
        };
        assert!(!agent.should_show_plan_chip(&appearance));
    }
    #[test]
    fn set_input_mode_vim_empty_prompt_switches_to_scrollback_and_j_selects_next() {
        crate::appearance::cache::set_simple_mode(true);
        let mut agent = make_agent();
        agent.vim_mode = true;
        agent.set_active_pane(ActivePane::Prompt, true);
        agent.set_input_mode(InputMode::Vim);
        assert_eq!(agent.active_pane, ActivePane::Scrollback);
        assert!(!agent.is_simple_mode());
        let registry = ActionRegistry::defaults();
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        let outcome = agent.handle_scrollback_key(&j, &registry);
        assert!(matches!(outcome, InputOutcome::Action(Action::SelectNext)));
    }
    #[test]
    fn set_input_mode_vim_nonempty_prompt_keeps_pane() {
        let mut agent = make_agent();
        agent.set_active_pane(ActivePane::Prompt, true);
        agent.prompt.set_text("draft");
        agent.set_input_mode(InputMode::Vim);
        assert_eq!(agent.active_pane, ActivePane::Prompt);
    }
    #[test]
    fn set_input_mode_simple_from_scrollback_leaves_pane_unchanged() {
        let mut agent = make_agent();
        agent.vim_mode = true;
        agent.set_active_pane(ActivePane::Scrollback, true);
        agent.set_input_mode(InputMode::Simple);
        assert_eq!(agent.active_pane, ActivePane::Scrollback);
        assert!(agent.is_simple_mode());
        let registry = ActionRegistry::defaults();
        let x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let outcome = agent.handle_scrollback_key(&x, &registry);
        assert_eq!(agent.active_pane, ActivePane::Scrollback);
        assert!(matches!(outcome, InputOutcome::Unchanged));
    }
    #[test]
    fn new_agent_respects_persisted_simple_mode_for_mode_and_pane() {
        crate::appearance::cache::set_simple_mode(true);
        let a1 = make_agent();
        assert!(a1.is_simple_mode());
        assert_eq!(a1.active_pane, ActivePane::Prompt);
        crate::appearance::cache::set_simple_mode(false);
        let a2 = make_agent();
        assert!(!a2.is_simple_mode());
        assert_eq!(a2.active_pane, ActivePane::Scrollback);
    }
    #[test]
    fn set_input_mode_reconciles_pane_orthogonal_to_active_modal_field() {
        let mut agent = make_agent();
        agent.set_active_pane(ActivePane::Prompt, true);
        agent.active_modal = None;
        agent.set_input_mode(InputMode::Vim);
        assert_eq!(agent.active_pane, ActivePane::Scrollback);
        assert!(agent.active_modal.is_none());
    }
    #[test]
    fn scrollback_j_with_vim_mode_off_forwards_to_prompt() {
        crate::appearance::cache::set_vim_mode(false);
        let mut agent = make_agent();
        agent.vim_mode = false;
        agent.set_active_pane(ActivePane::Scrollback, true);
        let registry = ActionRegistry::defaults();
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        let outcome = agent.handle_scrollback_key(&j, &registry);
        assert!(
            matches!(
                outcome,
                InputOutcome::ActionThenForward(Action::FocusPrompt)
            ),
            "vim-off: bare 'j' in scrollback must forward to prompt; got {outcome:?}"
        );
    }
    #[test]
    fn scrollback_j_with_vim_mode_on_selects_next() {
        crate::appearance::cache::set_vim_mode(true);
        let mut agent = make_agent();
        agent.vim_mode = true;
        agent.set_active_pane(ActivePane::Scrollback, true);
        let registry = ActionRegistry::defaults();
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        let outcome = agent.handle_scrollback_key(&j, &registry);
        assert!(
            matches!(outcome, InputOutcome::Action(Action::SelectNext)),
            "vim-on: bare 'j' in scrollback must dispatch SelectNext; got {outcome:?}"
        );
    }
    #[test]
    fn scrollback_arrow_down_works_in_both_modes() {
        let registry = ActionRegistry::defaults();
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut a_off = make_agent();
        a_off.vim_mode = false;
        a_off.set_active_pane(ActivePane::Scrollback, true);
        assert!(matches!(
            a_off.handle_scrollback_key(&down, &registry),
            InputOutcome::Action(Action::SelectNext)
        ));
        let mut a_on = make_agent();
        a_on.vim_mode = true;
        a_on.set_active_pane(ActivePane::Scrollback, true);
        assert!(matches!(
            a_on.handle_scrollback_key(&down, &registry),
            InputOutcome::Action(Action::SelectNext)
        ));
    }
}
#[cfg(test)]
mod plan_approval_enter_tests {
    use super::test_fixtures::make_agent;
    use super::*;
    use crate::views::plan_approval_view::PlanApprovalFocus;
    fn enter_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }
    fn stashed_text(text: &str) -> crate::views::prompt_widget::StashedPrompt {
        let mut stash = crate::views::prompt_widget::StashedPrompt::default();
        stash.text = text.to_owned();
        stash.cursor = text.len();
        stash
    }
    fn toast_text(agent: &AgentView) -> Option<&str> {
        agent.toast.as_ref().map(|(msg, _)| msg.as_str())
    }
    const APPROVE_REFUSAL: &str =
        "Run the slash command in the notes with Enter, or clear it, before approving.";
    const APPROVE_REFUSAL_COMMENTING: &str =
        "The comment in progress is a slash command: finish or discard it before approving.";
    fn agent_with_revise_prompt() -> AgentView {
        agent_with_revise_prompt_and_response().0
    }
    /// Like [`agent_with_revise_prompt`] but keeps the shell-side receiver so a test can prove nothing was sent.
    fn agent_with_revise_prompt_and_response() -> (
        AgentView,
        tokio::sync::oneshot::Receiver<
            distill_acp_lib::AcpResult<agent_client_protocol::ExtResponse>,
        >,
    ) {
        let mut agent = make_agent();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "test-session".into(),
            tool_call_id: "call-1".into(),
            plan_content: Some("# Plan\n\n## Step 1\nDo something".into()),
        };
        let mut pav = crate::views::plan_approval_view::PlanApprovalViewState::new(
            request,
            stashed_text(""),
            tx,
        );
        pav.focus = PlanApprovalFocus::Prompt;
        agent.plan_approval_view = Some(pav);
        agent.prompt.set_text("");
        (agent, rx)
    }
    #[test]
    fn empty_enter_on_revise_prompt_does_not_approve() {
        let mut agent = agent_with_revise_prompt();
        let outcome = agent.handle_plan_feedback_key(&enter_key());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            agent.plan_approval_view.is_some(),
            "empty Enter must leave plan approval open"
        );
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Type revision notes, or press a to approve.")
        );
    }
    #[test]
    fn enter_with_revision_text_requests_changes() {
        let mut agent = agent_with_revise_prompt();
        agent.prompt.set_text("please use auth middleware");
        let outcome = agent.handle_plan_feedback_key(&enter_key());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(agent.plan_approval_view.is_none());
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Plan revision sent.")
        );
    }
    #[test]
    fn empty_enter_with_pending_comments_still_requests_changes() {
        let mut agent = agent_with_revise_prompt();
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.comments.push(PlanComment {
                id: 1,
                line_range: 0..1,
                text: "nit".into(),
            });
        }
        let outcome = agent.handle_plan_feedback_key(&enter_key());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(agent.plan_approval_view.is_none());
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Plan revision sent.")
        );
    }
    #[test]
    fn a_on_empty_revise_prompt_approves() {
        let mut agent = agent_with_revise_prompt();
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let outcome = agent.handle_plan_feedback_key(&a);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(agent.plan_approval_view.is_none(), "`a` must approve");
        assert_ne!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Plan revision sent.")
        );
    }
    #[test]
    fn a_with_pending_comments_and_empty_freeform_approves() {
        let mut agent = agent_with_revise_prompt();
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.comments.push(PlanComment {
                id: 1,
                line_range: 0..1,
                text: "nit".into(),
            });
        }
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let outcome = agent.handle_plan_feedback_key(&a);
        assert!(
            agent.plan_approval_view.is_none(),
            "empty freeform + comments: `a` must approve with comments"
        );
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::Interject { .. })
        ));
    }
    #[test]
    fn a_with_nonempty_freeform_types_letter() {
        let mut agent = agent_with_revise_prompt();
        agent.prompt.set_text("notes");
        agent.prompt.set_cursor(agent.prompt.text().len());
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let _ = agent.handle_plan_feedback_key(&a);
        assert!(
            agent.plan_approval_view.is_some(),
            "non-empty freeform: `a` must type into the revision notes"
        );
        assert_eq!(agent.prompt.text(), "notesa");
    }
    #[test]
    fn tab_out_of_commenting_restores_freeform() {
        let mut agent = agent_with_revise_prompt();
        agent.prompt.set_text("keep my freeform notes");
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.stashed_feedback_prompt = Some(agent.prompt.stash());
            pav.commenting_range = Some(0..1);
            pav.focus = PlanApprovalFocus::Commenting;
        }
        agent.prompt.set_text("unsaved comment draft");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let _ = agent.handle_plan_feedback_key(&tab);
        assert_eq!(agent.prompt.text(), "keep my freeform notes");
        assert_eq!(
            agent.plan_approval_view.as_ref().map(|p| p.focus),
            Some(PlanApprovalFocus::Preview)
        );
    }
    #[test]
    fn reopen_plan_approval_does_not_clobber_session_draft() {
        let mut agent = agent_with_revise_prompt();
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.stashed_prompt = crate::views::prompt_widget::StashedPrompt {
                text: "session draft from mid-thinking".into(),
                cursor: 0,
                images: Vec::new(),
                chip_elements: Vec::new(),
                image_counter: 0,
                image_undo_stash: Vec::new(),
            };
        }
        agent.prompt.set_text("revision freeform");
        agent.reopen_plan_approval();
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .map(|p| p.stashed_prompt.text.as_str()),
            Some("session draft from mid-thinking"),
        );
        assert_eq!(agent.prompt.text(), "revision freeform");
        agent.abandon_plan();
        assert_eq!(agent.prompt.text(), "session draft from mid-thinking");
    }
    #[test]
    fn approve_includes_freeform_notes() {
        let mut agent = agent_with_revise_prompt();
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.stashed_prompt = crate::views::prompt_widget::StashedPrompt {
                text: "session draft".into(),
                cursor: 0,
                images: Vec::new(),
                chip_elements: Vec::new(),
                image_counter: 0,
                image_undo_stash: Vec::new(),
            };
        }
        agent.prompt.set_text("please also fix auth");
        let outcome = agent.approve_plan();
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::Interject { ref text, .. })
                if text.contains("please also fix auth")
        ));
        assert_eq!(
            agent.prompt.text(),
            "session draft",
            "approve restores session draft after including freeform"
        );
    }
    #[test]
    fn approve_does_not_duplicate_prefilled_session_images() {
        let mut agent = agent_with_revise_prompt();
        let session_img = crate::prompt_images::PastedImage {
            element_id: distill_ratatui_textarea::ElementId::from_raw(1),
            display_number: 1,
            mime_type: "image/png".into(),
            dimensions: Some((100, 80)),
            byte_len: 16,
            encoded_bytes: Some(vec![0u8; 16].into()),
            source_path: None,
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        };
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.stashed_prompt = crate::views::prompt_widget::StashedPrompt {
                text: "see [Image #1] ".into(),
                cursor: 0,
                images: vec![session_img.clone()],
                chip_elements: vec![crate::app::agent::ChipElement {
                    range: 4..14,
                    kind: crate::views::prompt_widget::KIND_IMAGE,
                    display: None,
                }],
                image_counter: 1,
                image_undo_stash: Vec::new(),
            };
        }
        let mut freeform_img = session_img;
        freeform_img.element_id = distill_ratatui_textarea::ElementId::from_raw(2);
        agent.prompt.set_text("see [Image #1] ");
        agent.prompt.set_images(vec![freeform_img]);
        agent.approve_plan();
        assert_eq!(agent.prompt.images.len(), 1);
        assert_eq!(
            agent
                .prompt
                .images
                .first()
                .unwrap_or_else(|| panic!("missing index"))
                .display_number,
            1
        );
    }
    #[test]
    fn approve_merges_new_freeform_image_despite_reused_display_number() {
        let mut agent = agent_with_revise_prompt();
        let session_img = crate::prompt_images::PastedImage {
            element_id: distill_ratatui_textarea::ElementId::from_raw(1),
            display_number: 1,
            mime_type: "image/png".into(),
            dimensions: Some((100, 80)),
            byte_len: 16,
            encoded_bytes: Some(vec![1u8; 16].into()),
            source_path: None,
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        };
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.stashed_prompt = crate::views::prompt_widget::StashedPrompt {
                text: "session [Image #1] ".into(),
                cursor: 0,
                images: vec![session_img],
                chip_elements: vec![crate::app::agent::ChipElement {
                    range: 8..18,
                    kind: crate::views::prompt_widget::KIND_IMAGE,
                    display: None,
                }],
                image_counter: 1,
                image_undo_stash: Vec::new(),
            };
        }
        agent.prompt.set_text("");
        let new_img = crate::prompt_images::PastedImage {
            element_id: distill_ratatui_textarea::ElementId::from_raw(0),
            display_number: 0,
            mime_type: "image/png".into(),
            dimensions: Some((100, 80)),
            byte_len: 16,
            encoded_bytes: Some(vec![9u8; 16].into()),
            source_path: None,
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        };
        agent
            .prompt
            .insert_image(new_img)
            .expect("paste freeform image after clear");
        assert_eq!(
            agent
                .prompt
                .images
                .first()
                .unwrap_or_else(|| panic!("missing index"))
                .display_number,
            1,
            "precondition: freeform counter reset reuses #1"
        );
        agent.approve_plan();
        assert_eq!(
            agent.prompt.images.len(),
            2,
            "new freeform image must merge beside session image"
        );
        let numbers: Vec<_> = agent
            .prompt
            .images
            .iter()
            .map(|i| i.display_number)
            .collect();
        assert!(
            numbers.contains(&1) && numbers.contains(&2),
            "got {numbers:?}"
        );
        assert!(
            agent.prompt.text().contains("[Image #2]"),
            "renumbered chip must appear in session draft text, got {:?}",
            agent.prompt.text()
        );
    }
    #[test]
    fn approve_strips_image_chips_from_interjection_text() {
        let mut agent = agent_with_revise_prompt();
        agent.prompt.set_text("also check auth ");
        let img = crate::prompt_images::PastedImage {
            element_id: distill_ratatui_textarea::ElementId::from_raw(0),
            display_number: 0,
            mime_type: "image/png".into(),
            dimensions: Some((100, 80)),
            byte_len: 16,
            encoded_bytes: Some(vec![0u8; 16].into()),
            source_path: None,
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        };
        agent
            .prompt
            .insert_image(img)
            .expect("insert freeform image chip");
        assert!(
            agent.prompt.text().contains("[Image #1]"),
            "precondition: chip in freeform text"
        );
        let outcome = agent.approve_plan();
        match outcome {
            InputOutcome::Action(Action::Interject { text, images }) => {
                assert!(
                    !text.contains("[Image #"),
                    "approve interjection must not leak image chip tokens, got {text:?}"
                );
                assert!(
                    text.contains("also check auth"),
                    "non-chip freeform text must still ship, got {text:?}"
                );
                assert!(
                    images.is_empty(),
                    "approve interjection stays text-only; images merge into session draft"
                );
            }
            other => panic!("expected Interject with freeform, got {other:?}"),
        }
        assert!(
            agent.prompt.text().contains("[Image #1]"),
            "merged freeform image must restore with a chip in session draft text, got {:?}",
            agent.prompt.text()
        );
        assert_eq!(
            agent.prompt.images.len(),
            1,
            "freeform image must merge into restored session draft"
        );
        let stashed = agent.prompt.stash();
        agent.prompt.restore(stashed);
        assert_eq!(agent.prompt.images.len(), 1);
        assert!(agent.prompt.text().contains("[Image #1]"));
    }
    #[test]
    fn a_on_image_only_freeform_approves() {
        let mut agent = agent_with_revise_prompt();
        let img = crate::prompt_images::PastedImage {
            element_id: distill_ratatui_textarea::ElementId::from_raw(0),
            display_number: 0,
            mime_type: "image/png".into(),
            dimensions: Some((100, 80)),
            byte_len: 16,
            encoded_bytes: Some(vec![0u8; 16].into()),
            source_path: None,
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        };
        agent
            .prompt
            .insert_image(img)
            .expect("insert freeform image chip");
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let outcome = agent.handle_plan_feedback_key(&a);
        assert!(
            agent.plan_approval_view.is_none(),
            "image-only freeform: `a` must approve (not type the letter)"
        );
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            agent.prompt.text().contains("[Image #1]"),
            "freeform image folds into restored session draft"
        );
    }
    #[test]
    fn image_only_freeform_enter_toasts_instead_of_empty_revision() {
        let mut agent = agent_with_revise_prompt();
        let img = crate::prompt_images::PastedImage {
            element_id: distill_ratatui_textarea::ElementId::from_raw(0),
            display_number: 0,
            mime_type: "image/png".into(),
            dimensions: Some((100, 80)),
            byte_len: 16,
            encoded_bytes: Some(vec![0u8; 16].into()),
            source_path: None,
            staged_temp_path: None,
            session_image_path: None,
            preview: crate::prompt_images::PromptImagePreview::default(),
        };
        agent
            .prompt
            .insert_image(img)
            .expect("insert freeform image chip");
        let outcome = agent.handle_plan_feedback_key(&enter_key());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(
            agent.plan_approval_view.is_some(),
            "image-only freeform must not cancel the plan with empty feedback"
        );
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Type revision notes, or press a to approve.")
        );
    }
    #[test]
    fn enter_with_builtin_slash_returns_send_prompt_and_keeps_review_open() {
        let (mut agent, mut rx) = agent_with_revise_prompt_and_response();
        let text = "/feedback grok does not understand plan mode";
        agent.prompt.set_text(text);
        let outcome = agent.handle_plan_feedback_key(&enter_key());
        match outcome {
            InputOutcome::Action(Action::SendPrompt(sent)) => assert_eq!(text, sent),
            other => {
                panic!("a complete builtin must dispatch as a prompt, got {other:?}")
            }
        }
        assert_eq!(
            text,
            agent.prompt.text(),
            "dispatch clears the composer, not the overlay"
        );
        assert!(
            agent.plan_approval_view.is_some(),
            "a slash command never decides the plan"
        );
        assert_eq!(
            Some(PlanApprovalFocus::Prompt),
            agent.plan_approval_view.as_ref().map(|pav| pav.focus)
        );
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "the shell must not see a revision"
        );
        assert_ne!(Some("Plan revision sent."), toast_text(&agent));
    }
    #[test]
    fn enter_with_unedited_slash_prefill_empties_session_draft() {
        let mut agent = agent_with_revise_prompt();
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.stashed_prompt = stashed_text("/feedback x");
        }
        agent.prompt.set_text("/feedback x");
        let outcome = agent.handle_plan_feedback_key(&enter_key());
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::SendPrompt(_))
        ));
        assert!(
            agent
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.stashed_prompt.is_effectively_empty()),
            "a consumed prefill must not be restored and re-run on close"
        );
    }
    #[test]
    fn enter_with_unknown_slash_still_revises() {
        let (mut agent, mut rx) = agent_with_revise_prompt_and_response();
        agent.prompt.set_text("/nope x");
        let outcome = agent.handle_plan_feedback_key(&enter_key());
        assert!(matches!(outcome, InputOutcome::Changed));
        assert!(agent.plan_approval_view.is_none());
        assert_eq!(Some("Plan revision sent."), toast_text(&agent));
        let raw = rx
            .try_recv()
            .expect("revision response must be sent")
            .expect("Ok");
        let parsed: serde_json::Value =
            serde_json::from_str(raw.0.get()).expect("revision response is JSON");
        assert_eq!(
            Some(&serde_json::json!("cancelled")),
            parsed.pointer("/outcome")
        );
        assert_eq!(
            Some(&serde_json::json!("/nope x")),
            parsed.pointer("/feedback")
        );
    }
    #[test]
    fn approve_with_builtin_slash_freeform_refuses() {
        let (mut agent, mut rx) = agent_with_revise_prompt_and_response();
        agent.prompt.set_text("/feedback x");
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.focus = PlanApprovalFocus::Preview;
        }
        let outcome = agent.approve_plan();
        assert!(
            matches!(outcome, InputOutcome::Changed),
            "no interjection may carry the command, got {outcome:?}"
        );
        assert!(agent.plan_approval_view.is_some());
        assert_eq!(
            Some(PlanApprovalFocus::Prompt),
            agent.plan_approval_view.as_ref().map(|pav| pav.focus),
            "refusal moves focus to the notes box so Enter runs the command"
        );
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(Some(APPROVE_REFUSAL), toast_text(&agent));
        assert_eq!("/feedback x", agent.prompt.text());
    }
    #[test]
    fn approve_in_commenting_focus_with_slash_comment_keeps_commenting() {
        let mut agent = agent_with_revise_prompt();
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.focus = PlanApprovalFocus::Commenting;
            pav.commenting_range = Some(0..1);
            pav.stashed_feedback_prompt = Some(stashed_text("notes"));
        }
        agent.prompt.set_text("/feedback x");
        let outcome = agent.approve_plan();
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(
            Some(PlanApprovalFocus::Commenting),
            agent.plan_approval_view.as_ref().map(|pav| pav.focus),
            "the composer still holds the in-progress comment"
        );
        assert_eq!(Some(APPROVE_REFUSAL_COMMENTING), toast_text(&agent));
    }
}
/// The mode indicator renders `plan_mode_pending.unwrap_or(plan_mode_active)`.
/// The shell's confirming `CurrentModeUpdate("default")` only arrives after the exit tool runs (and can be lost entirely).
/// Resolving the review with a decision must therefore optimistically clear the effective plan mode on BOTH decision paths (approve and abandon).
#[cfg(test)]
mod plan_approval_optimistic_mode_tests {
    use super::*;
    use crate::app::actions::PermissionLabel;
    use agent_client_protocol as acp;
    fn make_agent() -> AgentView {
        let mut agent = super::test_fixtures::make_agent();
        agent.post_turn_plan_review = true;
        agent
    }
    fn agent_in_plan_mode_with_approval() -> (
        AgentView,
        tokio::sync::oneshot::Receiver<distill_acp_lib::AcpResult<acp::ExtResponse>>,
    ) {
        let mut agent = make_agent();
        agent.plan_mode_active = true;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "test-session".into(),
            tool_call_id: "call-1".into(),
            plan_content: Some("# Plan\n\n## Step 1\nDo something".into()),
        };
        let pav = crate::views::plan_approval_view::PlanApprovalViewState::new(
            request,
            agent.prompt.stash(),
            tx,
        );
        agent.plan_approval_view = Some(pav);
        (agent, rx)
    }
    fn effective_plan_mode(agent: &AgentView) -> bool {
        agent.plan_mode_pending.unwrap_or(agent.plan_mode_active)
    }
    #[test]
    fn approve_plan_optimistically_clears_plan_mode() {
        let (mut agent, mut rx) = agent_in_plan_mode_with_approval();
        assert!(effective_plan_mode(&agent));
        agent.approve_plan();
        assert_eq!(agent.plan_mode_pending, Some(false));
        assert!(
            !effective_plan_mode(&agent),
            "indicator must leave plan mode immediately on approve, \
             not wait for the shell's CurrentModeUpdate"
        );
        let raw = rx
            .try_recv()
            .expect("approval response must be sent")
            .expect("Ok");
        let parsed: serde_json::Value = serde_json::from_str(raw.0.get()).unwrap();
        assert_eq!(
            parsed.pointer("/outcome"),
            Some(&serde_json::json!("approved"))
        );
    }
    /// Approve with review comments takes the early `Action::Interject` return; the optimistic clear must happen before that branch.
    #[test]
    fn approve_plan_with_comments_still_clears_plan_mode() {
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        if let Some(ref mut pav) = agent.plan_approval_view {
            pav.comments
                .push(crate::views::plan_approval_view::PlanComment {
                    id: 1,
                    line_range: 1..2,
                    text: "use the existing helper".into(),
                });
        }
        let outcome = agent.approve_plan();
        assert!(matches!(
            outcome,
            InputOutcome::Action(Action::Interject { .. })
        ));
        assert_eq!(agent.plan_mode_pending, Some(false));
        assert!(!effective_plan_mode(&agent));
    }
    #[test]
    fn abandon_plan_optimistically_clears_plan_mode() {
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        agent.abandon_plan();
        assert_eq!(agent.plan_mode_pending, Some(false));
        assert!(!effective_plan_mode(&agent));
    }
    fn plan_review_closed_rows(agent: &AgentView) -> Vec<(PlanReviewOutcome, PermissionLabel)> {
        agent
            .scrollback
            .session_events()
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::PlanReviewClosed {
                    outcome,
                    permission,
                } => Some((outcome, permission)),
                _ => None,
            })
            .collect()
    }
    #[test]
    fn close_plan_review_pushes_one_closed_row_only_for_decisions() {
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        agent.session.auto_mode = true;
        agent.approve_plan();
        assert_eq!(
            plan_review_closed_rows(&agent),
            vec![(PlanReviewOutcome::Approved, PermissionLabel::Auto)]
        );
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        agent.session.yolo_mode = true;
        agent.abandon_plan();
        assert_eq!(
            plan_review_closed_rows(&agent),
            vec![(PlanReviewOutcome::Abandoned, PermissionLabel::AlwaysApprove)]
        );
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        agent.send_plan_feedback(Some("tighten the rollout".into()));
        assert!(plan_review_closed_rows(&agent).is_empty());
        assert!(effective_plan_mode(&agent));
    }
    fn agent_with_post_turn_review() -> AgentView {
        let mut agent = make_agent();
        agent.post_turn_plan_review = true;
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.open_post_turn_plan_review();
        agent
    }
    #[test]
    fn stage_plan_mode_on_clears_abandoned_intent() {
        let mut agent = agent_with_post_turn_review();
        agent.stage_plan_mode(false);
        assert_eq!(
            agent.pending_post_turn_commit,
            Some(PostTurnPlanCommit::Abandoned)
        );
        agent.stage_plan_mode(true);
        assert_eq!(agent.plan_mode_pending, Some(true));
        assert!(
            agent.pending_post_turn_commit.is_none(),
            "re-enter must not leave Abandoned for a late Default"
        );
        assert!(agent.plan_approval_view.is_some());
        assert!(agent.kept_plan.is_kept());
    }
    #[test]
    fn late_default_during_staged_reentry_does_not_abandon() {
        let mut agent = agent_with_post_turn_review();
        agent.stage_plan_mode(false);
        agent.stage_plan_mode(true);
        crate::app::acp_handler::detect_plan_mode_change_replayed(
            &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
            &mut agent,
            false,
        );
        assert!(
            plan_review_closed_rows(&agent).is_empty(),
            "late Default during staged re-entry must not write Abandoned"
        );
        assert!(
            agent.kept_plan.is_kept(),
            "late Default during staged re-entry must keep the plan"
        );
        assert!(
            agent
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.is_after_turn()),
            "late Default during staged re-entry must leave approve/build mounted"
        );
        assert_eq!(agent.plan_mode_pending, Some(true));
    }
    #[test]
    fn stale_execute_plan_release_clears_approved_intent() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        agent.pending_post_turn_commit = Some(PostTurnPlanCommit::Approved);
        agent.release_stale_execute_plan_prompt(None);
        assert!(agent.execute_plan_prompt_id().is_none());
        assert!(
            agent.pending_post_turn_commit.is_none(),
            "dropped build id must not leave Approved for a later Default"
        );
        crate::app::acp_handler::detect_plan_mode_change_replayed(
            &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
            &mut agent,
            false,
        );
        assert!(
            !plan_review_closed_rows(&agent)
                .iter()
                .any(|(outcome, _)| *outcome == PlanReviewOutcome::Approved),
            "Default after a dropped build must not write approved"
        );
    }
    #[test]
    fn take_execute_plan_prompt_clears_approved_intent() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        agent.pending_post_turn_commit = Some(PostTurnPlanCommit::Approved);
        assert!(agent.take_execute_plan_prompt(Some("build-1")));
        assert!(agent.pending_post_turn_commit.is_none());
        assert!(agent.plan_approval_view.is_some());
    }
    #[test]
    fn create_plan_end_turn_opens_post_turn_review() {
        let agent = agent_with_post_turn_review();
        let pav = agent
            .plan_approval_view
            .as_ref()
            .expect("review after EndTurn");
        assert!(pav.is_after_turn(), "review must not hold a worker prompt");
        assert_eq!(pav.plan_content.as_deref(), Some("# Build it\n"));
        assert_eq!(pav.source, PlanReviewSource::Inline);
    }
    #[test]
    fn post_turn_approve_refreshes_when_the_keep_diverges() {
        let mut agent = agent_with_post_turn_review();
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Second body\n".to_owned()), None);
        assert!(matches!(agent.approve_plan(), InputOutcome::Changed));
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some(PLAN_CHANGED_ON_DISK_NOTICE),
        );
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .and_then(|pav| pav.plan_content.as_deref()),
            Some("# Second body\n"),
        );
        assert!(matches!(
            agent.approve_plan(),
            InputOutcome::Action(Action::ExecutePlan { plan_file_content, .. })
                if plan_file_content.starts_with("# Second body\n")
        ));
    }
    #[test]
    fn plan_content_with_review_skips_blank_and_already_appended_notes() {
        assert_eq!(
            AgentView::plan_content_with_review("# Build it\n".into(), None),
            "# Build it\n"
        );
        assert_eq!(
            AgentView::plan_content_with_review("# Build it\n".into(), Some("   ")),
            "# Build it\n"
        );
        assert_eq!(
            AgentView::plan_content_with_review(String::new(), Some("notes")),
            "notes"
        );
        let once = AgentView::plan_content_with_review("# Build it\n".into(), Some("notes"));
        assert_eq!(
            AgentView::plan_content_with_review(once.clone(), Some("notes")),
            once,
            "do not append the same notes twice"
        );
    }
    #[test]
    fn post_turn_approve_appends_review_notes_onto_plan_file_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kept.plan.md");
        std::fs::write(&path, "# Build it\n").expect("write");
        let mut agent = make_agent();
        agent.post_turn_plan_review = true;
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), Some(path));
        agent.open_post_turn_plan_review();
        agent.prompt.set_text("ship it behind a flag");
        match agent.approve_plan() {
            InputOutcome::Action(Action::ExecutePlan {
                plan_file_content,
                plan_file_uri,
            }) => {
                assert!(
                    plan_file_content.contains("# Build it\n"),
                    "plan body stays on planFileContent"
                );
                assert!(
                    plan_file_content.contains("ship it behind a flag"),
                    "approve must append notes so the worker path sees them"
                );
                assert_eq!(
                    plan_file_uri, None,
                    "omit keep URI so the daemon cannot prefer the on-disk file"
                );
            }
            other => panic!("expected ExecutePlan, got {other:?}"),
        }
        assert!(
            agent.plan_approval_view.is_some(),
            "review stays mounted until ExecutePlan is accepted"
        );
    }
    #[test]
    fn post_turn_approve_sends_plan_file_uri() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kept.plan.md");
        std::fs::write(&path, "# Build it\n").expect("write");
        let mut agent = make_agent();
        agent.post_turn_plan_review = true;
        agent.plan_mode_active = true;
        agent.kept_plan = crate::app::agent_view::KeptPlan::kept(
            Some("# Build it\n".to_owned()),
            Some(path.clone()),
        );
        agent.open_post_turn_plan_review();
        match agent.approve_plan() {
            InputOutcome::Action(Action::ExecutePlan { plan_file_uri, .. }) => {
                let expected = url::Url::from_file_path(&path)
                    .expect("file uri")
                    .to_string();
                assert_eq!(plan_file_uri.as_deref(), Some(expected.as_str()));
            }
            other => panic!("expected ExecutePlan, got {other:?}"),
        }
    }
    #[test]
    fn post_turn_approve_starts_a_new_build_turn() {
        let mut agent = agent_with_post_turn_review();
        assert!(matches!(
            agent.approve_plan(),
            InputOutcome::Action(Action::ExecutePlan { .. })
        ));
        assert!(
            agent.plan_approval_view.is_some(),
            "review stays mounted until ExecutePlan is accepted"
        );
        assert!(
            plan_review_closed_rows(&agent).is_empty(),
            "approved row must not land before dispatch accepts"
        );
        assert!(
            agent.kept_plan.is_kept(),
            "approve must keep the plan until ExecutePlan is known to have started"
        );
        assert!(
            agent.plan_mode_pending.is_none(),
            "post-turn approve must not leave Plan until the worker accepts the build"
        );
    }
    #[test]
    fn post_turn_revise_is_a_plain_plan_message() {
        let mut agent = agent_with_post_turn_review();
        assert!(matches!(
            agent.send_plan_feedback(Some("add a rollback".into())),
            InputOutcome::Action(Action::RevisePlan(text)) if text.contains("add a rollback")
        ));
        assert!(
            agent.plan_approval_view.is_some(),
            "review stays mounted until revise send is accepted"
        );
        assert!(
            agent.plan_mode_pending.is_none(),
            "revise stays in Plan; close_plan_review must not run"
        );
        assert!(
            agent.kept_plan.is_kept(),
            "revise must keep the plan so the next EndTurn can reopen review"
        );
        assert_eq!(
            agent.kept_plan.body(),
            Some("# Build it\n"),
            "content-only keeps stay so the next EndTurn can reopen review"
        );
    }
    fn user_prompt_texts(agent: &AgentView) -> Vec<String> {
        agent
            .scrollback
            .iter_entries()
            .filter_map(|(_, entry)| match &entry.block {
                crate::scrollback::RenderBlock::UserPrompt(block) => Some(block.text.clone()),
                _ => None,
            })
            .collect()
    }
    #[test]
    fn in_turn_inline_revise_drops_snapshot_without_a_kept_path() {
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Stale CreatePlan\n".to_owned()), None);
        assert!(agent.kept_plan.path().is_none());
        assert_eq!(
            Some(PlanReviewSource::Inline),
            agent.plan_approval_view.as_ref().map(|pav| pav.source)
        );
        agent.send_plan_feedback(Some("tighten the rollout".into()));
        assert!(
            agent.kept_plan.body().is_none(),
            "Inline revise must drop the snapshot so preview reads plan.md"
        );
    }
    #[test]
    fn in_turn_revise_in_minimal_mode_pushes_one_user_row() {
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        agent
            .prompt
            .set_screen_mode(crate::app::ScreenMode::Minimal);
        agent.send_plan_feedback(Some("tighten the rollout".into()));
        assert_eq!(
            user_prompt_texts(&agent),
            vec!["tighten the rollout".to_owned()],
            "in-turn notes never become a prompt; minimal mode still needs one live row"
        );
    }
    #[test]
    fn post_turn_revise_in_minimal_mode_does_not_push_a_user_row() {
        let mut agent = agent_with_post_turn_review();
        agent
            .prompt
            .set_screen_mode(crate::app::ScreenMode::Minimal);
        let outcome = agent.send_plan_feedback(Some("add a rollback".into()));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::RevisePlan(_))),
            "post-turn revise must dispatch a real prompt"
        );
        assert!(
            user_prompt_texts(&agent).is_empty(),
            "the send path echoes the revision; this function must not add a second row"
        );
    }
    #[test]
    fn post_turn_revise_restores_the_pre_review_composer_draft() {
        let mut agent = make_agent();
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.prompt.set_text("pre-review draft");
        agent
            .prompt
            .insert_image(crate::prompt_images::PastedImage {
                element_id: distill_ratatui_textarea::ElementId::from_raw(1),
                display_number: 1,
                mime_type: "image/png".into(),
                dimensions: Some((100, 80)),
                byte_len: 16,
                encoded_bytes: Some(vec![7u8; 16].into()),
                source_path: None,
                staged_temp_path: None,
                session_image_path: None,
                preview: crate::prompt_images::PromptImagePreview::default(),
            })
            .expect("insert session draft image");
        let draft_text = agent.prompt.text().to_owned();
        agent.open_post_turn_plan_review();
        assert!(matches!(
            agent.send_plan_feedback(Some("add a rollback".into())),
            InputOutcome::Action(Action::RevisePlan(text))
                if text.contains("add a rollback")
        ));
        agent.commit_post_turn_plan_revised();
        assert_eq!(agent.prompt.text(), draft_text);
        assert_eq!(agent.prompt.images.len(), 1);
        assert_eq!(
            agent
                .prompt
                .images
                .first()
                .and_then(|image| image.encoded_bytes.as_deref()),
            Some([7u8; 16].as_slice())
        );
    }
    #[test]
    fn empty_post_turn_revise_keeps_the_review_open() {
        let mut agent = agent_with_post_turn_review();
        assert!(matches!(
            agent.send_plan_feedback(Some("   ".into())),
            InputOutcome::Changed
        ));
        assert!(
            agent.plan_approval_view.is_some(),
            "empty revise must not drop approve/build"
        );
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Type revision notes, or press a to approve.")
        );
        assert!(agent.kept_plan.is_kept());
    }
    #[test]
    fn post_turn_review_stays_off_when_the_backend_cannot_execute() {
        let mut agent = make_agent();
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.post_turn_plan_review = false;
        agent.open_post_turn_plan_review();
        assert!(
            agent.plan_approval_view.is_none(),
            "Approve sends ExecutePlan; backends that do not implement it must not open review"
        );
    }
    #[test]
    fn post_turn_abandon_switches_mode_and_starts_no_turn() {
        let mut agent = agent_with_post_turn_review();
        assert!(matches!(
            agent.abandon_plan(),
            InputOutcome::Action(Action::SetPlanMode(crate::app::actions::PlanModeKind::Off))
        ));
        assert!(
            agent.plan_approval_view.is_some(),
            "review stays mounted until SetPlanMode(Off) is accepted"
        );
        assert!(
            plan_review_closed_rows(&agent).is_empty(),
            "abandoned row must not land before dispatch accepts"
        );
        assert!(
            agent.kept_plan.is_kept(),
            "abandon must keep the plan until session/set_mode is known to have started"
        );
        assert!(
            agent.plan_mode_pending.is_none(),
            "post-turn abandon must not flip pending off before SetPlanMode"
        );
    }
    #[test]
    fn post_turn_approved_commit_leaves_plan_without_dropping_build_id() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        agent.commit_post_turn_plan_approved();
        assert!(
            agent.plan_mode_pending.is_none(),
            "lost Default leaves Plan without claiming the user asked"
        );
        assert!(!agent.plan_mode_active);
        assert_eq!(agent.execute_plan_prompt_id(), Some("build-1"));
        assert_eq!(
            plan_review_closed_rows(&agent)
                .into_iter()
                .map(|(outcome, _)| outcome)
                .collect::<Vec<_>>(),
            vec![PlanReviewOutcome::Approved]
        );
    }
    #[test]
    fn abandoned_commit_after_default_confirm_does_not_restage_pending() {
        let mut agent = agent_with_post_turn_review();
        agent.plan_mode_pending = Some(false);
        crate::app::acp_handler::detect_plan_mode_change_replayed(
            &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
            &mut agent,
            false,
        );
        assert!(
            agent.plan_mode_pending.is_none(),
            "Default confirm already cleared pending; abandon must not restage Some(false)"
        );
        assert_eq!(
            plan_review_closed_rows(&agent)
                .into_iter()
                .map(|(outcome, _)| outcome)
                .collect::<Vec<_>>(),
            vec![PlanReviewOutcome::Abandoned]
        );
        assert!(!agent.kept_plan.is_kept());
        assert!(agent.plan_approval_view.is_none());
    }
    #[test]
    fn approved_commit_after_default_confirm_does_not_restage_pending() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        crate::app::acp_handler::detect_plan_mode_change_replayed(
            &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
            &mut agent,
            false,
        );
        assert!(
            agent.plan_mode_pending.is_none(),
            "Default confirm already cleared pending"
        );
        agent.commit_post_turn_plan_approved();
        agent.leave_plan_after_approved_build();
        assert!(
            agent.plan_mode_pending.is_none(),
            "confirmed Default must not restage pending as Some(false)"
        );
        let user_requested = agent.plan_mode_pending.is_some();
        crate::app::acp_handler::detect_plan_mode_change_replayed(
            &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("plan")),
            &mut agent,
            false,
        );
        assert!(
            !user_requested,
            "a later agent Plan entry must not look user-requested"
        );
        assert!(agent.plan_mode_active);
        assert!(agent.plan_mode_pending.is_none());
    }
    #[test]
    fn leave_plan_does_not_clobber_staged_plan_reentry() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        crate::app::acp_handler::detect_plan_mode_change_replayed(
            &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
            &mut agent,
            false,
        );
        agent.plan_mode_pending = Some(true);
        agent.commit_post_turn_plan_approved();
        agent.leave_plan_after_approved_build();
        assert_eq!(
            agent.plan_mode_pending,
            Some(true),
            "staged Plan re-entry must survive the settled-build leave"
        );
        let user_requested = agent.plan_mode_pending.is_some();
        crate::app::acp_handler::detect_plan_mode_change_replayed(
            &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("plan")),
            &mut agent,
            false,
        );
        assert!(
            user_requested,
            "CurrentModeUpdate(plan) after a staged re-entry must look user-requested"
        );
        assert!(agent.plan_mode_active);
        assert!(agent.plan_mode_pending.is_none());
    }
    #[test]
    fn abandon_and_revise_refuse_while_build_in_flight() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        assert!(matches!(agent.abandon_plan(), InputOutcome::Changed));
        assert!(agent.plan_approval_view.is_some());
        assert!(agent.kept_plan.is_kept());
        assert_eq!(agent.execute_plan_prompt_id(), Some("build-1"));
        assert!(plan_review_closed_rows(&agent).is_empty());
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Wait for the current turn to end before abandoning the plan.")
        );
        assert!(agent.plan_mode_pending.is_none());
        assert!(matches!(
            agent.send_plan_feedback(Some("add a rollback".into())),
            InputOutcome::Changed
        ));
        assert!(agent.plan_approval_view.is_some());
        assert!(agent.kept_plan.is_kept());
        assert_eq!(agent.execute_plan_prompt_id(), Some("build-1"));
        assert!(plan_review_closed_rows(&agent).is_empty());
        assert_eq!(
            agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
            Some("Wait for the current turn to end before revising the plan.")
        );
    }
    #[test]
    fn dismiss_in_turn_leaves_a_post_turn_review_and_its_comments() {
        let mut agent = agent_with_post_turn_review();
        agent
            .plan_approval_view
            .as_mut()
            .expect("review")
            .comments
            .push(crate::views::plan_approval_view::PlanComment {
                id: 1,
                line_range: 1..2,
                text: "keep this".to_owned(),
            });
        assert!(!agent.dismiss_in_turn_plan_review());
        let pav = agent.plan_approval_view.as_ref().expect("still open");
        assert_eq!(
            pav.comments.first().map(|comment| comment.text.as_str()),
            Some("keep this")
        );
    }
    #[test]
    fn dismiss_in_turn_closes_a_held_ext_review() {
        let (mut agent, _rx) = agent_in_plan_mode_with_approval();
        assert!(agent.dismiss_in_turn_plan_review());
        assert!(agent.plan_approval_view.is_none());
    }
    #[test]
    fn in_flight_build_does_not_open_post_turn_review() {
        let mut agent = make_agent();
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.set_execute_plan_prompt("build-1");
        agent.open_post_turn_plan_review();
        assert!(
            agent.plan_approval_view.is_none(),
            "re-entering Plan during a started build must not reopen approve/build"
        );
        assert!(
            agent.kept_plan.is_kept(),
            "open is a no-op; Default confirm forgets the keep"
        );
    }
    #[test]
    fn reconnect_finalize_clears_stale_execute_plan_id() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        agent.begin_session_reload(1);
        assert!(agent.finalize_reload_and_maybe_adopt(1, true, None));
        assert!(
            agent.execute_plan.is_none(),
            "a same-session reconnect without that build must drop the id"
        );
        assert!(
            agent.plan_approval_view.is_some(),
            "reload must not dismiss the post-turn review"
        );
        assert!(
            matches!(
                agent.abandon_plan(),
                InputOutcome::Action(Action::SetPlanMode(crate::app::actions::PlanModeKind::Off))
            ),
            "abandon must work after the lost ExecutePlan RPC"
        );
    }
    #[test]
    fn reconnect_finalize_keeps_id_when_adopting_same_build() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        agent.begin_session_reload(1);
        assert!(agent.finalize_reload_and_maybe_adopt(1, true, Some("build-1".into())));
        assert_eq!(agent.execute_plan_prompt_id(), Some("build-1"));
        assert!(agent.session.state.is_turn_running());
        assert!(
            matches!(agent.abandon_plan(), InputOutcome::Changed),
            "an adopted ExecutePlan must still refuse abandon"
        );
    }
    #[test]
    fn reconnect_finalize_clears_id_when_adopting_a_different_turn() {
        let mut agent = agent_with_post_turn_review();
        agent.set_execute_plan_prompt("build-1");
        agent.begin_session_reload(1);
        assert!(agent.finalize_reload_and_maybe_adopt(1, true, Some("other-turn".into())));
        assert!(agent.execute_plan.is_none());
        assert_eq!(
            agent.session.current_prompt_id.as_deref(),
            Some("other-turn")
        );
    }
    #[test]
    fn reconnect_finalize_lets_a_later_keep_open_review() {
        let mut agent = make_agent();
        agent.post_turn_plan_review = true;
        agent.plan_mode_active = true;
        agent.set_execute_plan_prompt("build-1");
        agent.begin_session_reload(1);
        assert!(agent.finalize_reload_and_maybe_adopt(1, true, None));
        agent.kept_plan = crate::app::agent_view::KeptPlan::kept(Some("# Next\n".to_owned()), None);
        agent.open_post_turn_plan_review();
        assert!(
            agent.plan_approval_view.is_some(),
            "a later keep must remount after the stale build id is dropped"
        );
    }
    #[test]
    fn leave_plan_pending_does_not_open_post_turn_review() {
        let mut agent = make_agent();
        agent.plan_mode_active = true;
        agent.plan_mode_pending = Some(false);
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.open_post_turn_plan_review();
        assert!(
            agent.plan_approval_view.is_none(),
            "EndTurn must not open approve/build while leaving Plan"
        );
        assert!(
            agent.kept_plan.is_kept(),
            "open is a no-op; the leave-Plan path forgets the keep"
        );
    }
    #[test]
    fn dismiss_plan_review_ui_leaves_in_turn_review() {
        let (mut agent, mut rx) = agent_in_plan_mode_with_approval();
        agent.forget_waiting_plan();
        assert!(
            agent
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.is_in_turn()),
            "mode change must not cancel a held in-turn review"
        );
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "exit_plan_mode must stay outstanding"
        );
    }
    #[test]
    fn capped_kept_plan_body_rejects_empty_and_oversized() {
        assert!(capped_kept_plan_body(String::new()).is_none());
        assert!(capped_kept_plan_body("   ".to_owned()).is_none());
        assert_eq!(
            Some("# Plan\n"),
            capped_kept_plan_body("# Plan\n".to_owned()).as_deref()
        );
        let over = "x".repeat(
            usize::try_from(MAX_KEPT_PLAN_FILE_BYTES.saturating_add(1)).expect("cap fits usize"),
        );
        assert!(capped_kept_plan_body(over).is_none());
    }
    #[test]
    fn post_turn_review_rereads_the_kept_file_after_revise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kept.plan.md");
        std::fs::write(&path, "# First plan\n").expect("write");
        let mut agent = make_agent();
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# First plan\n".to_owned()), None);
        agent.kept_plan = crate::app::agent_view::KeptPlan::kept(
            agent.kept_plan.body().map(str::to_owned),
            Some(path.clone()),
        );
        agent.open_post_turn_plan_review();
        assert!(matches!(
            agent.send_plan_feedback(Some("tighten the rollout".into())),
            InputOutcome::Action(Action::RevisePlan(_))
        ));
        agent.commit_post_turn_plan_revised();
        assert!(agent.kept_plan.body().is_none());
        assert_eq!(agent.kept_plan.path(), Some(path.as_path()));
        std::fs::write(&path, "# Revised plan\n").expect("rewrite");
        agent.open_post_turn_plan_review();
        let pav = agent
            .plan_approval_view
            .as_ref()
            .expect("review after revise");
        assert_eq!(pav.plan_content.as_deref(), Some("# Revised plan\n"));
    }
}
