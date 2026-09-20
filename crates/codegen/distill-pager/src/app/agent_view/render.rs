// Modified for Distill by Samuel Fajreldines, 2026.
//! Frame rendering for [`AgentView`]: the `draw` entry point plus shortcut hints.
use super::{
    ActivePane, AgentPane, AgentView, AgentViewLayout, BlockingCard, ComposerRoute, CtaPhase,
    EscStep, InlineMediaHitAreas, KeyOwner, MODE_BANNER_FADE_TICKS, PromptMode, ViewSurface,
    collect_citation_links, dropdown_content_inset, dropdown_items_width, record_dot_pulse,
    render_dropdown_chrome, supports_osc22,
};
use crate::actions::{ActionId, ActionRegistry};
use crate::key;
use crate::render::SafeBuf;
use crate::render::line_utils::truncate_line;
use crate::scrollback::block::BlockContent;
use crate::scrollback::layout::HorizontalLayout;
use crate::scrollback::render::ScratchBuffer;
use crate::scrollback::text_selection::{
    ResolvedSelectionBoundaries, ResolvedSelectionModel, render_active_selection_overlay,
    render_block_drag_overlay, render_persistent_selection_overlay,
};
use crate::theme::Theme;
use crate::views::agent::AgentViewLayoutParams;
use crate::views::btw_overlay::BTW_OVERLAY_ENTRY_IDX;
use crate::views::modal;
use crate::views::plan_approval_view::PlanApprovalFocus;
use crate::views::prompt_widget::{PromptBg, PromptFlag, PromptInfo, PromptStyle, mode_flags};
use crate::views::question_view::QUESTION_VIEW_HPAD;
use crate::views::shortcuts_bar::{HintItem, PendingHint, ShortcutsBar};
use crate::views::{agent, turn_status};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use std::collections::HashSet;
use std::time::Instant;
/// AppView-owned per-frame inputs to [`AgentView::draw`]: state the agent view cannot see itself (voice pipeline, Esc ownership, status row).
/// Grouped (mirroring `WelcomeRenderParams`) so the next app-level render fact extends this struct instead of every `draw` call site.
/// Tests take `Default` and override only what they exercise.
#[derive(Default)]
pub struct AppRenderParams<'a> {
    /// Voice feature available (shows the mic affordances).
    pub voice_available: bool,
    /// Mic open and streaming on the active surface; drives the recording row and the prompt voice overlay.
    pub voice_listening: bool,
    /// Interim transcript for the prompt overlay while dictating.
    pub voice_interim: Option<&'a str>,
    /// The status row this frame paints, or `Off` when this frame has none.
    pub status_line: crate::views::status_line::StatusLineFrame,
    pub workspace_dashboard_enabled: bool,
    /// Header chrome the dashboard adds when this view is its session overlay: the agent's title (omitted when the session is
    /// unnamed) and its `i/n` position in the overlay's cycle order. `position` is `None` outside the overlay or for an agent the
    /// dashboard filter hid; a lone agent gets `Some((1, 1))`, so the switcher gates on [`OverlayHeader::can_cycle`], not `is_some`.
    pub overlay_header: OverlayHeader<'a>,
    /// The footer's `Ctrl+X` label when this view stands in for another agent (a subagent's fullscreen takeover): the
    /// parent's resolved stop/archive/close action, which the child cannot compute from its own state.
    pub overlay_stop_label: Option<&'static str>,
}
/// What the dashboard overlay contributes to the header row (see [`AppRenderParams::overlay_header`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct OverlayHeader<'a> {
    pub title: Option<&'a str>,
    pub position: Option<(usize, usize)>,
}
impl OverlayHeader<'_> {
    /// The `i/n` position when `‹`/`›` have anything to cycle through.
    pub fn cycle_position(&self) -> Option<(usize, usize)> {
        self.position.filter(|(_, n)| *n > 1)
    }
    /// Whether the header shows the switcher and the footer its prev/next hint.
    pub fn can_cycle(&self) -> bool {
        self.cycle_position().is_some()
    }
}
/// What the bottom shortcuts bar renders this frame.
enum ShortcutsBarContent {
    /// A blocking surface's own keys, rendered as given.
    Surface(Vec<HintItem>),
    /// The focused pane's keys, trimmed to the compact bar with the cheatsheet hint appended.
    Pane(Vec<HintItem>),
    /// Nothing: the row belongs to a surface that paints it itself.
    Hidden,
}
impl AgentView {
    pub(crate) fn live_standalone_subagent_tokens(&self) -> u64 {
        self.subagent_sessions
            .values()
            .filter(|info| info.is_running() && info.attempt.workflow_run_id.is_none())
            .map(|info| {
                info.completed_attempt_tokens
                    .saturating_add(info.attempt.tokens_used.unwrap_or(0))
            })
            .fold(0, u64::saturating_add)
    }
    pub(crate) fn update_scrollback_selection_state(
        &mut self,
        model: ResolvedSelectionModel,
        boundaries: ResolvedSelectionBoundaries,
    ) {
        self.last_scrollback_selection_model = model;
        self.last_scrollback_selection_boundaries = boundaries;
    }
    fn clear_scrollback_selection_state(&mut self) {
        self.update_scrollback_selection_state(Default::default(), Default::default());
    }
    /// Keep [`Self::timeline_hover_preview`] in sync with [`Self::timeline_hover`].
    /// Called when hover changes (mouse Moved or rail rebuild under a stationary pointer) so render can borrow the cached text.
    pub(crate) fn sync_timeline_hover_preview(&mut self) {
        match self.timeline_hover {
            Some(crate::views::timeline::TimelineHit::Tick(turn_idx)) => {
                if self
                    .timeline_hover_preview
                    .as_ref()
                    .is_some_and(|(idx, _)| *idx == turn_idx)
                {
                    return;
                }
                self.timeline_hover_preview = self
                    .scrollback
                    .turn_preview(turn_idx)
                    .map(|text| (turn_idx, text));
            }
            _ => self.timeline_hover_preview = None,
        }
    }
    /// Shortcut hints for the plan-approval prompt/comment focus states.
    /// Shared by `draw` and the cheatsheet Current section.
    /// `Tab:plan` is omitted when there is nothing to open (empty approval and no line viewer) so the footer never advertises a dead key.
    fn plan_approval_shortcut_hints(
        &self,
        pav: &crate::views::plan_approval_view::PlanApprovalViewState,
    ) -> Vec<HintItem> {
        match pav.focus {
            PlanApprovalFocus::Commenting => {
                vec![
                    HintItem::new(key!(Enter), "save comment"),
                    HintItem::new(key!(Esc), "cancel"),
                ]
            }
            PlanApprovalFocus::Prompt => {
                let has_content = !pav.comments.is_empty() || !self.prompt.text().trim().is_empty();
                if has_content {
                    vec![
                        HintItem::new(key!(Enter), "request changes"),
                        HintItem::new(key!(Tab), "plan"),
                        HintItem::new(key!(Esc), "back"),
                    ]
                } else {
                    vec![
                        HintItem::new(key!('a'), "approve"),
                        HintItem::new(key!(Tab), "plan"),
                        HintItem::new(key!(Esc), "back"),
                    ]
                }
            }
            PlanApprovalFocus::Preview => {
                vec![
                    HintItem::new(key!('y'), "copy plan"),
                    HintItem::new(key!(Tab), "prompt"),
                ]
            }
        }
    }
    /// The one-line freeform answer row, growing with what the user types.
    fn freeform_editor_h(&self, inner_width: u16, cap: u16, style: &PromptStyle) -> u16 {
        let question_text_w = crate::views::question_view::inline_text_width(inner_width);
        self.prompt
            .desired_height(question_text_w, style, false, cap)
    }
    /// The `Esc` hint for the focused card, named by the rung the key actually takes ([`EscStep`]).
    fn card_esc_hint(&self) -> HintItem {
        HintItem::new(key!(Esc), self.card_esc().map_or("back", EscStep::label))
    }
    /// Shortcut hints for an open `ask_user_question` card.
    fn question_shortcut_hints(
        &self,
        qv: &crate::views::question_view::QuestionViewState,
    ) -> Vec<HintItem> {
        use crate::views::question_view::QuestionFocus;
        let esc = self.card_esc_hint();
        match qv.focus {
            QuestionFocus::InputMode if self.prompt.file_search_visible() => {
                vec![
                    HintItem::paired(key!(Up), key!(Down), "nav"),
                    HintItem::new(key!(Tab), "accept"),
                    HintItem::new(key!(Right), "drill"),
                    esc,
                ]
            }
            QuestionFocus::InputMode => vec![HintItem::new(key!(Enter), "submit"), esc],
            QuestionFocus::Navigation if qv.is_prompt_blocked() => {
                vec![HintItem::new(key!(Tab), "next answer"), esc]
            }
            QuestionFocus::Navigation => {
                vec![
                    HintItem::new(key!(Tab), "next answer"),
                    esc,
                    HintItem::new(key!('X'), "dismiss"),
                ]
            }
        }
    }
    fn permission_shortcut_hints(
        &self,
        perm: &crate::views::permission_view::PermissionViewState,
    ) -> Vec<HintItem> {
        use crate::views::permission_view::PermissionFocus;
        let perm_content_w = self
            .pane_areas
            .prompt
            .width
            .saturating_sub(QUESTION_VIEW_HPAD) as usize;
        let ctrl_f_hint = perm.has_collapsible_display(perm_content_w).then(|| {
            let label = if perm.args_expanded {
                "collapse"
            } else {
                "expand"
            };
            HintItem::new(key!('f', CONTROL), label)
        });
        match perm.focus {
            PermissionFocus::FollowupInput => {
                let mut hints = vec![HintItem::new(key!(Enter), "send")];
                hints.extend(ctrl_f_hint);
                hints.push(self.card_esc_hint());
                hints
            }
            PermissionFocus::PatternEdit => {
                let mut hints = vec![HintItem::new(key!(Enter), "save")];
                hints.extend(ctrl_f_hint);
                hints.push(self.card_esc_hint());
                hints
            }
            PermissionFocus::Options => {
                use crate::input::key::KeyShortcut;
                use crossterm::event::{KeyCode, KeyModifiers};
                let n = perm.options.len().min(9) as u8;
                let last_ch = char::from(b'0' + n.max(1));
                let last_key = KeyShortcut::new(KeyCode::Char(last_ch), KeyModifiers::NONE);
                let mut hints = vec![HintItem::paired(key!('1'), last_key, "select")];
                if perm.options.len() > 1 {
                    hints.push(HintItem::new(key!(Tab), "next option"));
                }
                if perm.has_adjustable_scope() {
                    hints.push(HintItem::paired(key!(Left), key!(Right), "scope"));
                }
                if perm.has_editable_bash_pattern() {
                    hints.push(HintItem::new(key!('e'), "edit pattern"));
                }
                hints.extend(ctrl_f_hint);
                hints.push(HintItem::new(key!('o', CONTROL), "always-approve"));
                hints.push(HintItem::new(key!('c', CONTROL), "cancel"));
                hints.push(self.card_esc_hint());
                hints
            }
        }
    }
    /// Returns the *exact* hints the bottom shortcuts bar would render right now.
    /// Single source of truth for context-sensitive shortcuts (pane, overlays, sub-modes, selection state, turn running, plan/queue).
    /// Both the bar renderer and the Ctrl+. cheatsheet Current section delegate here, so the two stay identical in the active context.
    pub fn current_shortcut_hints(&self, registry: &ActionRegistry) -> Vec<HintItem> {
        match self.shortcuts_bar_content(registry) {
            ShortcutsBarContent::Surface(hints) | ShortcutsBarContent::Pane(hints) => hints,
            ShortcutsBarContent::Hidden => vec![],
        }
    }
    fn plan_approval_bar(
        &self,
        pav: &crate::views::plan_approval_view::PlanApprovalViewState,
    ) -> ShortcutsBarContent {
        let hints = self.plan_approval_shortcut_hints(pav);
        if hints.is_empty() {
            ShortcutsBarContent::Hidden
        } else {
            ShortcutsBarContent::Surface(hints)
        }
    }
    /// The bar names the surface the keys actually reach, so it asks [`AgentView::key_owner`] rather than keeping an order of its own.
    fn shortcuts_bar_content(&self, registry: &ActionRegistry) -> ShortcutsBarContent {
        use crate::views::shortcuts_bar::HintItem;
        match self.key_owner() {
            KeyOwner::LineViewer => self.line_viewer_bar(),
            KeyOwner::BlockViewer => ShortcutsBarContent::Surface(
                self.block_viewer
                    .as_ref()
                    .map(|viewer| viewer.shortcuts_hints())
                    .unwrap_or_default(),
            ),
            KeyOwner::Card(BlockingCard::Permission) => ShortcutsBarContent::Surface(
                self.focused_permission()
                    .map(|perm| self.permission_shortcut_hints(perm))
                    .unwrap_or_default(),
            ),
            KeyOwner::PlanApproval => self
                .plan_approval_view
                .as_ref()
                .map_or(ShortcutsBarContent::Hidden, |pav| {
                    self.plan_approval_bar(pav)
                }),
            KeyOwner::Card(BlockingCard::CancelTurn) => ShortcutsBarContent::Surface(vec![
                HintItem::paired(key!('1'), key!('4'), "select"),
                HintItem::new(key!(Tab), "next choice"),
                HintItem::new(key!(Enter), "confirm"),
                self.card_esc_hint(),
            ]),
            KeyOwner::Card(BlockingCard::Question) => ShortcutsBarContent::Surface(
                self.focused_question()
                    .map(|qv| self.question_shortcut_hints(qv))
                    .unwrap_or_default(),
            ),
            KeyOwner::Card(BlockingCard::McpElicitation) => ShortcutsBarContent::Surface(vec![
                HintItem::new(key!(Enter), "accept / toggle"),
                HintItem::new(key!('d'), "decline"),
                HintItem::new(key!('c', CONTROL), "cancel"),
                HintItem::new(key!(Tab), "next field"),
                self.card_esc_hint(),
            ]),
            KeyOwner::Pane => ShortcutsBarContent::Pane(self.normal_pane_hints(registry)),
        }
    }
    /// An open line viewer paints its own hints over this row further down `draw`, so the bar is silent.
    /// The two exceptions, where the viewer defers: the plan-approval prompt, whose keys the viewer's intercept forwards, and a casual comment draft.
    fn line_viewer_bar(&self) -> ShortcutsBarContent {
        use crate::views::shortcuts_bar::HintItem;
        if let Some(pav) = self
            .plan_approval_view
            .as_ref()
            .filter(|pav| pav.focus != PlanApprovalFocus::Preview)
        {
            return self.plan_approval_bar(pav);
        }
        if self.is_casual_commenting() {
            return ShortcutsBarContent::Surface(vec![
                HintItem::new(key!(Enter), "save comment"),
                HintItem::new(key!(Esc), "cancel"),
            ]);
        }
        ShortcutsBarContent::Hidden
    }
    /// Shared "normal pane" hints: the flag computation, `build_hints`, and the queue hint.
    /// Single source of truth for the two former duplicated blocks in `current_shortcut_hints` and `draw`.
    fn normal_pane_hints(&self, registry: &ActionRegistry) -> Vec<HintItem> {
        let fold_label = self.selected_fold_label();
        let is_editing = matches!(self.prompt_mode, PromptMode::EditingQueued { .. });
        let selected_entry = self
            .scrollback
            .selected()
            .and_then(|idx| self.scrollback.entry(idx));
        let selected_is_group_header = self
            .scrollback
            .selected()
            .is_some_and(|idx| self.scrollback.entry_content_hidden_by_group(idx));
        let (selected_supports_copy, selected_meta_label, selected_supports_fullscreen) =
            if self.active_pane == ActivePane::Catalog {
                (false, None, self.catalog.selected_entry().is_some())
            } else if self.active_pane == ActivePane::Tasks {
                let has_selected = self.tasks.selected_task_id().is_some_and(|tid| {
                    self.session
                        .bg_tasks
                        .get(tid)
                        .is_some_and(|t| !t.stdout.is_empty())
                });
                let has_any_selected = self.tasks.selected_task_id().is_some();
                (has_selected, None, has_any_selected)
            } else if self.active_pane == ActivePane::Scrollback {
                let is_bg_task = selected_entry.is_some_and(|e| {
                    matches!(e.block, crate::scrollback::block::RenderBlock::BgTask(_))
                });
                if is_bg_task {
                    let has_stdout = selected_entry
                        .and_then(|e| {
                            if let crate::scrollback::block::RenderBlock::BgTask(b) = &e.block {
                                Some(&b.task_id)
                            } else {
                                None
                            }
                        })
                        .is_some_and(|tid| {
                            self.session
                                .bg_tasks
                                .get(tid)
                                .is_some_and(|t| !t.stdout.is_empty())
                        });
                    (has_stdout, None, true)
                } else {
                    let is_viewable_subagent = selected_entry.is_some_and(|e| {
                        if let crate::scrollback::block::RenderBlock::Subagent(ref sb) = e.block {
                            self.subagent_views.contains_key(&sb.child_session_id)
                        } else {
                            false
                        }
                    });
                    (
                        !selected_is_group_header
                            && selected_entry.is_some_and(|e| e.block.supports_copy()),
                        selected_entry
                            .and_then(|e| e.block.copy_meta_label())
                            .filter(|_| !selected_is_group_header),
                        selected_entry.is_some_and(|e| e.block.supports_fullscreen())
                            || is_viewable_subagent,
                    )
                }
            } else {
                (
                    !selected_is_group_header
                        && selected_entry.is_some_and(|e| e.block.supports_copy()),
                    selected_entry
                        .and_then(|e| e.block.copy_meta_label())
                        .filter(|_| !selected_is_group_header),
                    selected_entry.is_some_and(|e| e.block.supports_fullscreen()),
                )
            };
        let can_demote = self.surface() == ViewSurface::Root
            && self
                .session
                .tracker
                .running_execute_tool_call_id()
                .is_some();
        let selected_can_kill = if self.surface() == ViewSurface::ChildTakeover
            || self.active_pane == ActivePane::Catalog
        {
            false
        } else if self.active_pane == ActivePane::Dock {
            self.dock_items()
                .get(self.dock_cursor)
                .copied()
                .is_some_and(|item| self.dock_stop_action(item).is_some())
        } else if self.active_pane == ActivePane::Tasks {
            self.tasks
                .selected_task_id()
                .and_then(|tid| self.session.bg_tasks.get(tid))
                .is_some_and(|t| {
                    t.status == crate::app::agent::BgTaskStatus::Running && !t.pending_kill
                })
        } else if self.active_pane == ActivePane::Scrollback {
            !selected_is_group_header
                && selected_entry
                    .and_then(|e| {
                        if let crate::scrollback::block::RenderBlock::BgTask(b) = &e.block {
                            Some(&b.task_id)
                        } else {
                            None
                        }
                    })
                    .and_then(|tid| self.session.bg_tasks.get(tid))
                    .is_some_and(|t| {
                        t.status == crate::app::agent::BgTaskStatus::Running && !t.pending_kill
                    })
        } else {
            false
        };
        let thinking_label = self.scrollback.thinking_fold_label();
        let selected_is_user_prompt = selected_entry.is_some_and(|e| e.block.is_user_prompt());
        let selected_is_agent_message = selected_entry.is_some_and(|e| e.block.is_agent_message());
        let mut hints = agent::build_hints(
            self.active_pane,
            self.parked_card()
                .map_or_else(agent::prompt_focus_hint, BlockingCard::focus_hint),
            &self.prompt,
            registry,
            is_editing,
            fold_label,
            if self.active_pane == ActivePane::Dock {
                self.dock_enter_label()
            } else {
                self.scrollback.selected_group_header_fold_label()
            },
            "prompt",
            thinking_label,
            if matches!(self.active_pane, ActivePane::Tasks | ActivePane::Dock) {
                self.tasks.show_done()
            } else {
                self.todo.show_done()
            },
            selected_supports_copy,
            selected_meta_label,
            selected_supports_fullscreen,
            can_demote,
            selected_can_kill,
            self.multiline_mode,
            self.vim_mode,
            self.surface(),
            (self.session.state.is_turn_running() || self.wake_turn_active())
                && !self.renders_parked(),
            !self.visible_queue_is_empty(),
            self.queue.mutation(),
            selected_is_user_prompt,
            selected_is_agent_message,
            crate::terminal::terminal_context().prefer_alt_enter_newline(),
            self.scrollback_search.as_ref(),
        );
        if (self.queue.is_visible() || !self.visible_queue_is_empty())
            && self.active_pane != ActivePane::Queue
            && !matches!(self.prompt_mode, PromptMode::EditingQueued { .. })
            && let Some(def) = registry.find(ActionId::ToggleQueue)
        {
            hints.push(def.hint());
        }
        if self.in_dashboard_overlay {
            hints.insert(
                0,
                HintItem::new(
                    registry
                        .find(ActionId::DashboardOverlayStop)
                        .map(|def| def.default_key)
                        .unwrap_or(key!('x', CONTROL)),
                    self.overlay_stop_label(),
                ),
            );
            if self.overlay_can_cycle {
                hints.insert(
                    0,
                    HintItem::paired(key!('[', CONTROL), key!(']', CONTROL), "prev/next agent"),
                );
            }
            hints.insert(0, HintItem::new(key!('\\', CONTROL), "dashboard"));
        }
        hints
    }
    /// The overlay footer's `Ctrl+X` label: the parent's resolved action when this view is a takeover, else this agent's own
    /// readiness under workspace dashboards, else the v1 `stop`.
    fn overlay_stop_label(&self) -> &'static str {
        self.overlay_stop_label.unwrap_or_else(|| {
            if self.workspace_dashboard_enabled {
                crate::app::dispatch::dashboard_stop_readiness(self)
                    .action()
                    .label()
            } else {
                "stop"
            }
        })
    }
    pub fn should_show_tip(&mut self) -> bool {
        false
    }
    /// Walking and copying the answer rows, counter first when the card holds more than one question.
    fn answer_footer_hints(
        qv: &crate::views::question_view::QuestionViewState,
        hint_style: Style,
        hint_key: Style,
    ) -> Vec<Span<'static>> {
        let mut left_spans: Vec<Span<'static>> = Vec::new();
        if qv.questions.len() > 1 {
            let counter = format!("[{}/{}] ", qv.active_tab + 1, qv.questions.len());
            left_spans.push(Span::styled(counter, hint_style));
        }
        left_spans.push(Span::styled("\u{2191}/\u{2193}", hint_key));
        left_spans.push(Span::styled(" navigate", hint_style));
        if qv.questions.len() > 1 {
            left_spans.push(Span::styled(" \u{b7} ", hint_style));
            left_spans.push(Span::styled("\u{2190}/\u{2192}", hint_key));
            left_spans.push(Span::styled(" question", hint_style));
        }
        if !qv.is_prompt_blocked() {
            left_spans.push(Span::styled(" \u{b7} ", hint_style));
            left_spans.push(Span::styled("y", hint_key));
            left_spans.push(Span::styled(" copy", hint_style));
        }
        left_spans
    }
    /// `area` is the screen region assigned to this agent view.
    /// When a tracing overlay is visible, this is smaller than `f.area()`.
    #[allow(clippy::too_many_arguments)]
    /// Render the agent into `area`.
    /// `in_dashboard_overlay` is `true` when this view is being rendered inside the dashboard's session-overlay.
    /// It appends `Ctrl+\\:dashboard` (and, when the overlay header can cycle, the `Ctrl+[/]:prev/next agent` chip) to the bottom shortcuts bar.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        registry: &ActionRegistry,
        scratch: &mut ScratchBuffer,
        pending_hint: Option<PendingHint>,
        overlay_focused: bool,
        banner: super::BannerSlotParams<'_>,
        bundle_state: &crate::app::bundle::BundleState,
        in_dashboard_overlay: bool,
        link_spans_out: &mut Vec<distill_ratatui_inline::LinkSpan>,
        app_params: AppRenderParams<'_>,
    ) -> (
        Option<(u16, u16)>,
        Option<crate::terminal::overlay::PostFlush>,
    ) {
        let AppRenderParams {
            voice_available,
            voice_listening,
            voice_interim,
            status_line,
            workspace_dashboard_enabled,
            overlay_header,
            overlay_stop_label,
        } = app_params;
        self.scrollback.begin_frame();
        self.in_dashboard_overlay = in_dashboard_overlay;
        self.overlay_can_cycle = overlay_header.can_cycle();
        self.workspace_dashboard_enabled = workspace_dashboard_enabled;
        self.overlay_stop_label = overlay_stop_label;
        let super::BannerSlotParams {
            height: banner_height,
            announcements: banner_announcements,
            hidden_ids: hidden_announcement_ids,
            privacy_banner,
            mouse_pos,
            tip,
        } = banner;
        self.session_banner_active = crate::views::announcements::first_session_announcement(
            banner_announcements,
            hidden_announcement_ids,
        )
        .is_some();
        self.privacy_banner.active = privacy_banner;
        self.pinned_upgrade_cta_live =
            crate::views::announcements::promo_cta(banner_announcements, hidden_announcement_ids)
                .is_some_and(|(owner, _, _)| !crate::views::announcements::is_dismissible(owner));
        self.frame_occluder_rects.clear();
        self.hit_model_tier_change.clear();
        self.clear_scrollback_selection_state();
        self.refresh_prompt_suggestion_gate();
        let theme = Theme::current();
        let link_active_style = Style::default()
            .fg(theme.link_fg)
            .add_modifier(ratatui::style::Modifier::UNDERLINED | ratatui::style::Modifier::BOLD);
        self.note_terminal_size((area.width, area.height));
        #[allow(unused_assignments)]
        let mut scrollback_inline_media = Vec::new();
        #[allow(unused_assignments)]
        let mut scrollback_diagram_affordances: Vec<
            crate::scrollback::render::DiagramAffordancePlacement,
        > = Vec::new();
        if self.inline_media_active
            && (self.image_viewer.is_some()
                || self.video_viewer.is_some()
                || self.gboom.is_some()
                || self.block_viewer.is_some()
                || self.extensions_modal.is_some()
                || self.feedback_modal.is_some()
                || self.agents_modal.is_some()
                || self.btw_state.is_some()
                || self.line_viewer.is_some()
                || self.active_modal.is_some())
        {
            self.inline_media_active = false;
            distill_shell::util::with_locked_stderr(|stderr| {
                for &id in self.inline_media_ids.values() {
                    let clear = crate::terminal::image::clear_kitty_image(id);
                    let _ = std::io::Write::write_all(stderr, clear.as_bytes());
                }
            });
            self.inline_media_ids.clear();
            self.inline_media_iterm_emitted.clear();
        }
        if let Some(ref child_sid) = self.active_subagent.clone() {
            if let Some(esc) = self.take_own_inline_media_clear_escapes() {
                distill_shell::util::with_locked_stderr(|stderr| {
                    let _ = std::io::Write::write_all(stderr, esc.as_bytes());
                });
            }
            self.hit_announcement_hide.clear();
            self.hit_announcement_cta.clear();
            self.hit_upgrade_cta.clear();
            self.hit_model_tier_change.clear();
            self.hit_dashboard.clear();
            self.hit_overlay_prev.clear();
            self.hit_overlay_next.clear();
            self.privacy_banner.clear_hits();
            self.take_dock_row_request();
            return self.draw_subagent_fullscreen(
                &child_sid.clone(),
                area,
                buf,
                registry,
                scratch,
                pending_hint,
                &theme,
                bundle_state,
                in_dashboard_overlay.then(|| super::subagent_takeover::InheritedOverlay {
                    header: overlay_header,
                    stop_label: self.overlay_stop_label(),
                }),
            );
        }
        if let Some(esc) = self.take_subagent_inline_media_clear_escapes() {
            distill_shell::util::with_locked_stderr(|stderr| {
                let _ = std::io::Write::write_all(stderr, esc.as_bytes());
            });
        }
        let appearance = self.scrollback.appearance().clone();
        let layout_cfg = &appearance.scrollback.layout;
        let scrollbar_cfg = &appearance.scrollback.scrollbar;
        let model_id = self
            .session
            .models
            .current_model_name()
            .unwrap_or_else(|| "unknown".to_string());
        let effective_plan = self.plan_mode_pending.unwrap_or(self.plan_mode_active);
        let casual_commenting = self.is_casual_commenting();
        let prompt_focused = if self.plan_approval_view.is_some() {
            self.plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.focus != PlanApprovalFocus::Preview)
        } else if casual_commenting {
            true
        } else {
            self.active_pane == AgentPane::Prompt && !overlay_focused
        };
        let prompt_style = PromptStyle {
            focused: prompt_focused,
            show_prefix: appearance.prompt.show_prefix,
            vpad_top: 1,
            compact: appearance.prompt.compact,
            chrome: true,
            chrome_pad_left: layout_cfg.block_pad_left,
            chrome_pad_right: layout_cfg.block_pad_right,
            bg: PromptBg::Default,
            accent_color_override: if let Some(c) = self.prompt_input_mode.accent_color(&theme) {
                Some(c)
            } else if effective_plan || casual_commenting {
                Some(theme.accent_plan)
            } else {
                None
            },
            border_color_override: if effective_plan || casual_commenting {
                crate::render::color::blend_color(theme.bg_base, theme.accent_plan, 0.4)
                    .or(Some(theme.accent_plan))
            } else {
                None
            },
            prefix_override: if let Some(p) = self.prompt_input_mode.prefix_override(&theme) {
                Some(p)
            } else if casual_commenting
                || self
                    .plan_approval_view
                    .as_ref()
                    .is_some_and(|pav| pav.focus == PlanApprovalFocus::Commenting)
            {
                Some((
                    if crate::glyphs::is_legacy_windows_console() {
                        "\u{2022} "
                    } else {
                        "\u{25CF} "
                    },
                    theme.accent_plan,
                ))
            } else {
                None
            },
            placeholder_when_focused: false,
            placeholder_override: if let Some(ph) = self
                .prompt_input_mode
                .placeholder_override(self.multiline_mode)
            {
                Some(ph)
            } else if casual_commenting
                || self
                    .plan_approval_view
                    .as_ref()
                    .is_some_and(|pav| pav.focus == PlanApprovalFocus::Commenting)
            {
                Some("Type your comment...")
            } else if self
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.focus == PlanApprovalFocus::Prompt)
            {
                Some("Type revision notes...")
            } else {
                None
            },
            show_accent_line: false,
            show_borders: true,
            title: self.prompt_caption(),
            image_preview: !self.resize_hides_prompt_preview(),
        };
        let next = crate::views::session_title::rename_source_title_raw(self)
            .map(crate::views::session_title::sanitize_display_text);
        if self.prompt.slash_current_title() != next.as_deref() {
            self.prompt
                .set_slash_current_title(next.map(|s| s.into_owned()));
            if self.prompt.slash_open() {
                self.prompt.refresh_slash(&self.session.models);
            }
        }
        let compact = appearance.prompt.compact;
        let inner_width = AgentViewLayout::inner_width(area, layout_cfg, compact);
        let banner_height = if banner_height > 0 {
            if let Some(tip_text) = tip {
                if self.session_banner_active {
                    banner_height
                } else {
                    banner_height.max(crate::tips::render::tip_height(
                        inner_width.saturating_sub(crate::tips::render::HINT_INSET),
                        tip_text,
                    ))
                }
            } else {
                banner_height
            }
        } else {
            banner_height
        };
        let banner_height = if privacy_banner {
            banner_height.max(crate::views::privacy_banner::height(inner_width))
        } else {
            banner_height
        };
        let tip_row_visible =
            self.ephemeral_tip_renderable(area.height) && self.ephemeral_tip.is_active();
        let banner_height = banner_height.max(u16::from(tip_row_visible));
        let max_prompt_height = area.height / 2;
        let base_prompt_height = if !prompt_focused && appearance.prompt.collapse_unfocused {
            self.prompt
                .desired_height(inner_width, &prompt_style, true, max_prompt_height)
                .min(prompt_style.vpad_top + 1 + prompt_style.info_block(true))
        } else {
            self.prompt
                .desired_height(inner_width, &prompt_style, true, max_prompt_height)
        };
        let overlay_content_w = inner_width.saturating_sub(QUESTION_VIEW_HPAD) as usize;
        let slot_card = self.blocking_card();
        let permission_view_h = if slot_card == Some(BlockingCard::Permission) {
            if let Some(perm) = self.permission_queue.front() {
                crate::views::permission_view::permission_view_height(
                    perm,
                    area.height,
                    overlay_content_w,
                )
            } else {
                0
            }
        } else {
            0
        };
        let question_view_h = if slot_card == Some(BlockingCard::Question) {
            if let Some(ref mut qv) = self.question_view {
                crate::views::question_view::question_view_height(
                    qv,
                    area.height,
                    overlay_content_w,
                )
            } else {
                0
            }
        } else {
            0
        };
        let elicitation_view_h = if slot_card == Some(BlockingCard::McpElicitation) {
            if let Some(ref ev) = self.elicitation_view {
                crate::views::elicitation_view::elicitation_view_height(
                    ev,
                    area.height,
                    overlay_content_w,
                )
            } else {
                0
            }
        } else {
            0
        };
        let rewind_view_h =
            if permission_view_h == 0 && question_view_h == 0 && elicitation_view_h == 0 {
                if let Some(ref rw) = self.rewind_state {
                    crate::views::rewind::rewind_overlay_height(&rw.phase, area.height)
                } else {
                    0
                }
            } else {
                0
            };
        let cancel_turn_view_h =
            if slot_card == Some(BlockingCard::CancelTurn) && rewind_view_h == 0 {
                modal::cancel_turn_panel_height(area.height)
            } else {
                0
            };
        let jump_view_h = if !self.jump_slot_taken() {
            if let Some(ref js) = self.jump_state {
                crate::views::jump::jump_overlay_height(js, area.height)
            } else {
                0
            }
        } else {
            0
        };
        let is_question_input_mode = self
            .question_view
            .as_ref()
            .map(|qv| qv.focus == crate::views::question_view::QuestionFocus::InputMode)
            .unwrap_or(false);
        let question_input_style = PromptStyle {
            focused: true,
            show_prefix: true,
            vpad_top: 0,
            chrome: false,
            chrome_pad_left: 0,
            chrome_pad_right: 0,
            bg: PromptBg::Panel(theme.bg_visual),
            accent_color_override: None,
            border_color_override: None,
            prefix_override: None,
            placeholder_when_focused: false,
            placeholder_override: None,
            compact: false,
            show_accent_line: false,
            show_borders: false,
            title: None,
            image_preview: !self.resize_hides_prompt_preview(),
        };
        let inline_prompt_max = ((area.height as u32) / 3).clamp(3, 15) as u16;
        let question_prompt_body_h = if question_view_h == 0 || !is_question_input_mode {
            0
        } else {
            self.freeform_editor_h(inner_width, inline_prompt_max, &question_input_style)
        };
        let is_permission_followup = self.permission_queue.front().is_some_and(|p| {
            p.focus == crate::views::permission_view::PermissionFocus::FollowupInput
        });
        let perm_inline_prompt_max = ((area.height as u32) / 3).clamp(3, 15) as u16;
        let perm_inline_prompt_h: u16 = if permission_view_h > 0 && is_permission_followup {
            let style = PromptStyle {
                focused: true,
                show_prefix: false,
                vpad_top: 0,
                chrome: false,
                chrome_pad_left: 0,
                chrome_pad_right: 0,
                bg: PromptBg::Panel(theme.bg_visual),
                accent_color_override: None,
                border_color_override: None,
                prefix_override: None,
                placeholder_when_focused: false,
                placeholder_override: None,
                compact: false,
                show_accent_line: false,
                show_borders: false,
                title: None,
                image_preview: !self.resize_hides_prompt_preview(),
            };
            let perm_text_w = crate::views::permission_view::inline_text_width(inner_width);
            self.prompt
                .desired_height(perm_text_w, &style, false, perm_inline_prompt_max)
                .max(1)
        } else {
            0
        };
        let question_footer_h: u16 = if question_view_h > 0 { 3 } else { 0 };
        let prompt_height = if permission_view_h > 0 {
            if is_permission_followup && perm_inline_prompt_h > 1 {
                permission_view_h + perm_inline_prompt_h.saturating_sub(1)
            } else {
                permission_view_h
            }
        } else if question_view_h > 0 {
            let freeform_offset = if is_question_input_mode { 1u16 } else { 0 };
            question_view_h.saturating_sub(freeform_offset)
                + question_prompt_body_h
                + question_footer_h
        } else if elicitation_view_h > 0 {
            elicitation_view_h
        } else if rewind_view_h > 0 {
            rewind_view_h
        } else if jump_view_h > 0 {
            jump_view_h
        } else if cancel_turn_view_h > 0 {
            cancel_turn_view_h
        } else {
            base_prompt_height
        };
        let prompt_height =
            prompt_height.max(prompt_style.vpad_top + 1 + prompt_style.info_block(true));
        let prompt_height = if self.composer_route() == ComposerRoute::Hidden {
            0
        } else {
            prompt_height
        };
        {
            use crate::app::agent::PENDING_KILL_TIMEOUT_SECS;
            let now = Instant::now();
            for task in self.session.bg_tasks.values_mut() {
                if let Some(requested) = task.kill_requested_at
                    && now.duration_since(requested).as_secs() >= PENDING_KILL_TIMEOUT_SECS
                {
                    task.pending_kill = false;
                    task.kill_requested_at = None;
                }
            }
            for info in self.subagent_sessions.values_mut() {
                if let Some(requested) = info.attempt.kill_requested_at
                    && now.duration_since(requested).as_secs() >= PENDING_KILL_TIMEOUT_SECS
                {
                    info.attempt.pending_kill = false;
                    info.attempt.kill_requested_at = None;
                }
            }
        }
        self.tasks.sync(
            &self.session.bg_tasks,
            &self.subagent_sessions,
            &self.session.scheduled_tasks,
            &self.workflow_runs,
        );
        if self.active_pane == ActivePane::Tasks && !self.tasks.is_visible() {
            self.active_pane = ActivePane::Scrollback;
        }
        self.catalog.sync_from_bundle(bundle_state);
        if self.active_pane == ActivePane::Catalog && !self.catalog.is_visible() {
            self.active_pane = ActivePane::Scrollback;
        }
        self.catalog.sync_from_bundle(bundle_state);
        if self.active_pane == ActivePane::Catalog && !self.catalog.is_visible() {
            self.active_pane = ActivePane::Scrollback;
        }
        let viewer_open = self.active_subagent.is_some();
        let dock_on = crate::views::dock::enabled()
            && !viewer_open
            && area.height > agent::SHORT_TERMINAL_ROWS;
        self.dock_on = dock_on;
        let tasks_height = if viewer_open || dock_on {
            0
        } else {
            self.tasks.desired_height(area.height)
        };
        let catalog_height = if viewer_open {
            0
        } else {
            self.catalog.desired_height(area.height)
        };
        let todo_height = if viewer_open {
            0
        } else {
            self.todo.desired_height(area.height)
        };
        self.sync_queue_pane();
        if dock_on {
            self.queue.overlay.visible =
                self.dock_queued_expanded && self.visible_held_queue_len() > 0;
        }
        if self.active_pane == ActivePane::Queue && !self.queue.is_visible() {
            self.active_pane = ActivePane::Scrollback;
        }
        if dock_on && self.active_pane == ActivePane::Tasks {
            self.active_pane = ActivePane::Scrollback;
        }
        let queue_height = if dock_on {
            0
        } else {
            self.queue.desired_height()
        };
        let drain_blocked = self.drain_blocked();
        let dock_covers_cues = self.dock_covers_idle_cues(dock_on);
        let turn_status_drain_blocked = if dock_covers_cues {
            false
        } else {
            drain_blocked
        };
        let watchers = self.watchers();
        let parked = self.renders_parked();
        let turn_status_watchers = if dock_covers_cues {
            crate::views::turn_status::Watchers::default()
        } else {
            watchers
        };
        let turn_status_parked = if dock_covers_cues { false } else { parked };
        let wake_display_state = self.wake_display_state();
        let display_state = wake_display_state.unwrap_or(&self.session.state);
        let send_now_gap = self.send_now_awaiting_current() && display_state.is_idle();
        let status_state = if send_now_gap {
            crate::app::agent::AgentState::TurnRunning
        } else {
            display_state.clone()
        };
        let turn_status_height = if turn_status::should_show(
            &status_state,
            turn_status_drain_blocked,
            self.session_starting_since,
            turn_status_watchers,
            turn_status_parked,
        ) {
            1
        } else {
            0
        };
        let prompt_gap = if appearance.prompt.compact
            || (turn_status_height > 0 && !appearance.turn_status.gap)
            || area.height <= agent::SHORT_TERMINAL_ROWS
        {
            0
        } else {
            1
        };
        let voice_recording_height = if voice_listening { 1 } else { 0 };
        let _tool_usage_height = 0u16;
        let btw_height =
            crate::views::btw_overlay::btw_panel_height(self.btw_state.as_ref(), inner_width);
        let cta_height = match &self.plugin_cta.phase {
            _ if privacy_banner => 0,
            CtaPhase::Hidden => 0,
            CtaPhase::Matched { .. } if self.prompt.text().trim().is_empty() => 0,
            _ => 1,
        };
        let follow_ups_height = u16::from(self.follow_ups.is_some());
        let mut dock_data = (dock_on && !self.dock_hidden).then(|| self.dock_snapshot());
        let dock_height = dock_data
            .as_ref()
            .map_or(0, crate::views::dock::desired_height);
        self.take_dock_row_request();
        self.dock_shown = dock_height > 0;
        if !self.dock_shown && self.active_pane == ActivePane::Dock {
            self.active_pane = ActivePane::Scrollback;
        }
        let timeline_width = crate::views::timeline::rail_width(
            appearance.show_timeline,
            self.surface(),
            area.width,
            self.scrollback.turn_count(),
        );
        let mut layout_params = AgentViewLayoutParams {
            area,
            layout_cfg: *layout_cfg,
            scrollbar_cfg: *scrollbar_cfg,
            timeline_width,
            prompt_height,
            tasks_height,
            catalog_height,
            todo_height,
            queue_height,
            btw_height,
            turn_status_height,
            banner_height,
            cta_height,
            follow_ups_height,
            dock_height,
            prompt_gap,
            voice_recording_height,
            shortcuts_height: 1,
            status_line_height: status_line.height(),
            compact,
        };
        if question_view_h > 0 {
            layout_params.prompt_height =
                prompt_height.min(AgentViewLayout::rows_available_for_prompt(layout_params));
        }
        let mut layout = AgentViewLayout::compute(layout_params);
        let search_active =
            self.scrollback_search.is_some() && self.active_pane == AgentPane::Scrollback;
        let search_reserved_rows =
            Self::search_reserved_rows(layout.scrollback.height, search_active);
        if search_reserved_rows > 0 {
            layout.scrollback.height -= search_reserved_rows;
            layout.scrollback_content.height = layout
                .scrollback_content
                .height
                .saturating_sub(search_reserved_rows);
        }
        let overlay_blocks_rail_hover = self.jump_state.is_some()
            || self.rewind_state.is_some()
            || self.blocking_card().is_some()
            || self.block_viewer.is_some();
        if layout.timeline_width > 0 {
            self.sync_pending_user_input_marks();
            self.scrollback.set_cwd(Some(self.session.cwd.clone()));
            let _ = self.sync_inline_edit_layout(layout.scrollback_content.width);
            self.scrollback.prepare_layout(
                layout.scrollback_content.width,
                layout.scrollback_content.height,
            );
            let viewport = crate::views::timeline::RailViewport {
                active: self.scrollback.active_turn_for_viewport(),
                up_target: self.scrollback.turn_above_viewport_top(),
                down_target: self.scrollback.turn_below_viewport_top(),
                at_bottom: !self.scrollback.has_content_below(),
            };
            match crate::views::timeline::compute_rail(
                layout.scrollback,
                layout.timeline_x,
                self.scrollback.turn_count(),
                viewport,
            ) {
                Some(rail) => {
                    self.timeline_rail = Some(rail);
                    if overlay_blocks_rail_hover {
                        self.timeline_hover = None;
                        self.timeline_hover_preview = None;
                    } else {
                        let (col, row) = self.last_mouse_pos;
                        let new_hover = self.timeline_rail.as_ref().and_then(|r| r.hit(col, row));
                        if new_hover != self.timeline_hover {
                            self.timeline_hover = new_hover;
                            self.sync_timeline_hover_preview();
                        }
                    }
                }
                None => {
                    self.timeline_rail = None;
                    self.timeline_hover = None;
                    self.timeline_hover_preview = None;
                    layout = AgentViewLayout::compute(AgentViewLayoutParams {
                        timeline_width: 0,
                        ..layout_params
                    });
                    if search_reserved_rows > 0 {
                        layout.scrollback.height -= search_reserved_rows;
                        layout.scrollback_content.height = layout
                            .scrollback_content
                            .height
                            .saturating_sub(search_reserved_rows);
                    }
                    self.scrollback.prepare_layout(
                        layout.scrollback_content.width,
                        layout.scrollback_content.height,
                    );
                }
            }
        } else {
            self.timeline_rail = None;
            self.timeline_hover = None;
            self.timeline_hover_preview = None;
        }
        if let Some(data) = &mut dock_data {
            if layout.dock.height > 0 {
                data.max_rows =
                    crate::views::dock::MaxRows::new(data.max_rows.get().min(layout.dock.height));
            }
            self.sync_dock_hover_from_pointer(layout.dock);
            data.hovered = self.dock_hovered;
            let (col, row) = self.last_mouse_pos;
            data.stop_hovered = crate::views::dock::hovered_stop_button_rect(layout.dock, data)
                .is_some_and(|hit| hit.rect.contains((col, row).into()));
        }
        if let Some(dock) = &dock_data {
            let body = crate::views::dock::queue_body_rect(layout.dock, dock);
            if body.height > 0 {
                layout.queue = body;
            }
        }
        agent::fill_background(buf, area, layout_cfg, compact, &theme);
        let mut status_line_link_spans: Vec<distill_ratatui_inline::LinkSpan> = Vec::new();
        if let Some(padding) = status_line.padding()
            && layout.status_line.height > 0
        {
            if let Some(width) =
                crate::views::status_line::inner_width(layout.status_line.width, padding)
            {
                self.last_status_line_size = Some(crate::views::status_line::RowSize {
                    cols: width,
                    lines: layout.status_line.height,
                });
            }
            if let Some(display) = status_line.display() {
                status_line_link_spans = crate::views::status_line::render_status_line(
                    buf,
                    layout.status_line,
                    display,
                    padding,
                    &theme,
                );
            }
        }
        use crate::views::agent_status::AgentStatusBar;
        use crate::views::context_bar;
        let bg = Style::default().bg(theme.bg_base);
        let dim = theme.dim().bg(theme.bg_base);
        let faint = theme.faint().bg(theme.bg_base);
        let mut status = AgentStatusBar::new(&theme);
        if !dock_on {
            let task_counts = self.tasks.status_counts(
                &self.session.bg_tasks,
                &self.subagent_sessions,
                &self.session.scheduled_tasks,
                &self.workflow_runs,
            );
            if let Some(line) = crate::views::agent_status::task_status_line(
                task_counts,
                &theme,
                self.hit_bg_status.hovered,
            ) {
                status.push("bg_tasks", line);
            }
        }
        if self.should_show_plan_chip(&appearance) {
            let mut plan_style = Style::default().fg(theme.accent_plan).bg(theme.bg_base);
            if self.hit_plan_button.hovered {
                plan_style = plan_style.add_modifier(ratatui::style::Modifier::BOLD);
            }
            status.push("plan", Line::from(Span::styled("plan", plan_style)));
        }
        if let Some(ref goal) = self.goal_state {
            let tick = self.tasks.tick_count() as usize;
            let active_subagent_tokens = self.live_standalone_subagent_tokens();
            status.push(
                "goal",
                crate::views::agent_status::goal_status_line(
                    goal,
                    &theme,
                    self.hit_goal_status.hovered,
                    tick,
                    self.context_state.as_ref().map(|c| c.used),
                    active_subagent_tokens,
                ),
            );
        }
        if let Some(mcp_line) = self.mcp_init_progress.as_ref().and_then(|p| {
            crate::views::agent_status::mcp_status_line(p, self.scrollback.animation_tick(), &theme)
        }) {
            status.push("mcp", mcp_line);
        }
        #[cfg(feature = "local-workspace")]
        if self.chat_kind || self.app_chat_mode {
            let label = self
                .workspace_mode
                .status_label(self.workspace_mode_cli_locked);
            let mut mode_style = Style::default().fg(theme.accent_user).bg(theme.bg_base);
            if self.workspace_mode_cli_locked {
                mode_style = mode_style.add_modifier(ratatui::style::Modifier::DIM);
            }
            status.push(
                "workspace_mode",
                Line::from(Span::styled(label, mode_style)),
            );
        }
        let ctx_used = self.context_state.as_ref().map(|c| c.used);
        let model_window = self.session.models.get_context_window();
        let ctx_total = self
            .context_state
            .as_ref()
            .and_then(|c| (c.total > 0).then_some(c.total))
            .or(model_window);
        if let Some(ctx_line) = context_bar::context_bar_line_for_session(
            ctx_used,
            ctx_total,
            self.hit_context.hovered,
            &theme,
            self.chat_kind,
        ) {
            status.push("context", ctx_line);
        }
        let hover_or = |hovered: bool, resting: Style| {
            if hovered {
                bg.fg(theme.text_primary)
            } else {
                resting
            }
        };
        if let Some((cur, total)) = overlay_header.cycle_position() {
            status.push(
                "switcher",
                Line::from(vec![
                    Span::styled(
                        crate::glyphs::chevron_left(),
                        hover_or(self.hit_overlay_prev.hovered, faint),
                    ),
                    Span::styled(format!(" {cur}/{total} "), bg.fg(theme.gray)),
                    Span::styled(
                        crate::glyphs::chevron(),
                        hover_or(self.hit_overlay_next.hovered, faint),
                    ),
                ]),
            );
        }
        let dashboard_available = in_dashboard_overlay
            || self
                .prompt
                .slash_controller
                .registry()
                .dashboard_dispatchable();
        if dashboard_available {
            status.push(
                "dashboard",
                Line::from(Span::styled(
                    "[Dashboard]",
                    hover_or(self.hit_dashboard.hovered, bg.fg(theme.gray)),
                )),
            );
        }
        if let Some(url) = self.highlighted_link_url() {
            const LOCATION_FLOOR: u16 = 24;
            const LINK_MIN: u16 = 12;
            let max_len = status
                .room_for_front(layout.status_bar.width)
                .saturating_sub(LOCATION_FLOOR);
            if max_len >= LINK_MIN {
                let display = crate::util::truncate_to_width(&url, max_len as usize).into_owned();
                let link_style = Style::default().fg(theme.link_fg).bg(theme.bg_base);
                status.push_front("link_url", Line::from(Span::styled(display, link_style)));
            }
        }
        let areas = status.render(buf, layout.status_bar);
        self.hit_bg_status.rect = areas.get("bg_tasks").copied();
        self.hit_goal_status.rect = areas.get("goal").copied();
        self.hit_context.rect = areas.get("context").copied();
        self.hit_credits.rect = areas.get("credits").copied();
        self.hit_plan_button.rect = areas.get("plan").copied();
        let dropdown_open = self.prompt.any_dropdown_open();
        self.hit_dashboard
            .set_unless_dropdown(areas.get("dashboard").copied(), dropdown_open);
        let switcher_rect = areas.get("switcher").copied();
        self.hit_overlay_prev
            .set_unless_dropdown(switcher_rect.map(|r| Rect { width: 1, ..r }), dropdown_open);
        self.hit_overlay_next.set_unless_dropdown(
            switcher_rect.map(|r| Rect {
                x: r.x + r.width.saturating_sub(1),
                width: 1,
                ..r
            }),
            dropdown_open,
        );
        let short = crate::util::display_location_path(&self.session.cwd);
        let left_budget = areas
            .values()
            .map(|r| r.x)
            .min()
            .map(|min_x| min_x.saturating_sub(layout.status_bar.x).saturating_sub(1))
            .unwrap_or(layout.status_bar.width);
        let upgrade_cta =
            crate::views::announcements::promo_cta(banner_announcements, hidden_announcement_ids);
        let upgrade_reserve = upgrade_cta.map_or(0u16, |(_, label, _)| {
            1 + crate::views::announcements::upgrade_cta_reserve(label, None)
        });
        let location_budget = left_budget.saturating_sub(upgrade_reserve);
        use unicode_width::UnicodeWidthStr;
        let mut location: Vec<Span> = Vec::new();
        let lazy_git = crate::git_info::cwd_git_info_lazy(&self.session.cwd);
        let branch = self
            .current_branch
            .clone()
            .or_else(|| lazy_git.as_ref().and_then(|i| i.branch.clone()))
            .map(crate::views::location::branch_label);
        if let Some(branch) = branch {
            location.push(Span::styled(branch, dim));
            location.push(Span::styled(" ", bg));
        }
        let show_worktree_label = self.is_worktree
            || self.session.is_worktree
            || lazy_git.as_ref().is_some_and(|i| i.is_worktree);
        if show_worktree_label {
            location.push(crate::views::location::worktree_badge(&theme).patch_style(bg));
        }
        if let Some(profile) = distill_sandbox::profile_name() {
            location.push(Span::styled(
                format!("sandbox:{profile} "),
                bg.fg(theme.warning),
            ));
        }
        let prefix_width: u16 = location.iter().map(|s| s.width() as u16).sum();
        let path_width = short.width() as u16;
        let title = overlay_header.title.and_then(|title| {
            const PATH_MIN: u16 = 12;
            let sep_width = crate::views::agent_status::separator(&theme).width() as u16;
            let location_min = prefix_width + path_width.min(PATH_MIN) + sep_width;
            let title_cap = (location_budget / 2).min(location_budget.saturating_sub(location_min));
            let title = crate::util::truncate_to_width(title, title_cap as usize);
            (!title.is_empty()).then(|| title.into_owned())
        });
        let path_style = hover_or(
            self.hit_cwd.hovered,
            if title.is_some() {
                dim
            } else {
                bg.fg(theme.text_secondary)
            },
        );
        location.push(Span::styled(short, path_style));
        let mut parts: Vec<Span> = Vec::new();
        let mut path_offset: u16 = prefix_width;
        if let Some(title) = title {
            let sep = crate::views::agent_status::separator(&theme);
            path_offset += (title.width() + sep.width()) as u16;
            parts.push(Span::styled(title, bg.fg(theme.text_secondary)));
            parts.push(sep);
        }
        parts.extend(location);
        let cwd_line = truncate_line(Line::from(parts), location_budget as usize);
        let cwd_width = cwd_line.width() as u16;
        buf.set_line_safe(
            layout.status_bar.x,
            layout.status_bar.y,
            &cwd_line,
            cwd_width,
        );
        let path_x = layout.status_bar.x + path_offset;
        let visible_path_width = path_width.min(cwd_width.saturating_sub(path_offset));
        self.hit_cwd.rect = (visible_path_width > 0).then_some(Rect {
            x: path_x,
            y: layout.status_bar.y,
            width: visible_path_width,
            height: 1,
        });
        let mut upgrade_cta_rect = None;
        if let Some((_owner, label, _url)) = upgrade_cta {
            let avail = left_budget.saturating_sub(cwd_width);
            if avail > 1 {
                let cta_x = layout.status_bar.x + cwd_width;
                buf.set_span(
                    cta_x,
                    layout.status_bar.y,
                    &Span::styled(" ", Style::default().bg(theme.bg_base)),
                    1,
                );
                upgrade_cta_rect = crate::views::announcements::render_cta_button(
                    buf,
                    &theme,
                    cta_x + 1,
                    layout.status_bar.y,
                    avail - 1,
                    label,
                    None,
                    self.hit_upgrade_cta.hovered,
                );
            }
        }
        self.hit_upgrade_cta
            .set_unless_dropdown(upgrade_cta_rect, dropdown_open);
        let mut inline_edit_cursor: Option<(u16, u16)> = None;
        let sticky_gap_row: Option<u16>;
        {
            self.sync_pending_user_input_marks();
            self.scrollback.set_cwd(Some(self.session.cwd.clone()));
            let inline_edit_dim_from =
                self.sync_inline_edit_layout(layout.scrollback_content.width);
            self.scrollback.prepare_layout(
                layout.scrollback_content.width,
                layout.scrollback_content.height,
            );
            let rewind_dim_from = self.rewind_dim_from_entry().or(inline_edit_dim_from);
            let sb_focused = self.active_pane == ActivePane::Scrollback && !overlay_focused;
            let search_highlight = if search_active {
                self.scrollback_search
                    .as_ref()
                    .and_then(|s| s.highlight_regex())
            } else {
                None
            };
            self.ensure_media_link_paths();
            let sb_rendered = crate::scrollback::ScrollbackPane::new()
                .active(sb_focused)
                .with_mouse_pos(self.last_mouse_pos)
                .with_dim_from(rewind_dim_from)
                .with_hovered_entry(self.hovered_entry)
                .with_search_highlight(search_highlight)
                .with_media_paths(self.media_link_paths.clone())
                .render_with_scratch_and_selection_boundaries(
                    layout.scrollback_content,
                    buf,
                    &self.scrollback,
                    scratch,
                );
            let sb_output = sb_rendered.output;
            sticky_gap_row = sb_output.sticky_gap_row;
            self.update_scrollback_selection_state(
                sb_output.selection_model.clone(),
                sb_rendered.selection_boundaries,
            );
            self.reclamp_drag_head_post_render(false);
            if self.inline_edit.is_some() {
                let cursor = self.render_inline_edit(buf, layout.scrollback_content);
                if self.rewind_state.is_none() {
                    inline_edit_cursor = cursor;
                }
            }
            if search_reserved_rows > 0
                && let Some(search) = self.scrollback_search.as_ref()
            {
                let reserved_top = layout.scrollback.y + layout.scrollback.height;
                let bar_y = reserved_top + (search_reserved_rows - 1);
                if search_reserved_rows >= 2 {
                    crate::views::picker::render_divider(
                        buf,
                        layout.scrollback.x,
                        reserved_top,
                        layout.scrollback.width,
                        &theme,
                        None,
                    );
                }
                let query = search.query();
                let counter = match search.current_index() {
                    Some(i) => Some(format!("{}/{}", i + 1, search.match_count())),
                    None if search.has_error() => Some("bad pattern".to_string()),
                    None if !query.is_empty() => Some("no matches".to_string()),
                    None => None,
                };
                let counter_width = counter
                    .as_deref()
                    .map_or(0, |text| UnicodeWidthStr::width(text) as u16);
                let search_layout =
                    crate::views::picker::search_bar_layout(layout.scrollback.width, counter_width);
                let leading_query;
                let (rendered_query, viewport) = if search.is_composing() {
                    (
                        query,
                        Some(search.query_viewport(search_layout.input_width())),
                    )
                } else {
                    leading_query =
                        crate::render::line_utils::truncate_str(query, search_layout.input_width());
                    (leading_query.as_str(), None)
                };
                crate::views::picker::render_search_bar_with_viewport(
                    buf,
                    layout.scrollback.x,
                    bar_y,
                    search_layout,
                    &theme,
                    rendered_query,
                    search.is_composing(),
                    !search.is_composing(),
                    None,
                    viewport.unwrap_or(distill_ratatui_textarea::SingleLineViewport {
                        visible_byte_range: 0..rendered_query.len(),
                        cursor_display_column: 0,
                    }),
                );
                if let Some(counter) = counter
                    && search_layout.trailing_width() > 0
                {
                    let w = UnicodeWidthStr::width(counter.as_str()) as u16;
                    if layout.scrollback.width > w {
                        buf.set_string(
                            layout.scrollback.x + layout.scrollback.width - w,
                            bar_y,
                            &counter,
                            Style::default().fg(theme.gray),
                        );
                    }
                }
            }
            self.last_link_overlay = sb_output.link_overlay;
            scrollback_inline_media = sb_output.inline_media;
            scrollback_diagram_affordances = sb_output.diagram_affordances;
            if self.visible_link_map.is_stale(self.scrollback.generation()) {
                let citation_links =
                    collect_citation_links(&self.scrollback, &sb_output.selection_model);
                self.visible_link_map.rebuild(
                    self.scrollback.generation(),
                    &self.last_link_overlay,
                    citation_links,
                );
                self.scrollback_visible_link_count = self.visible_link_map.len();
            } else {
                self.visible_link_map
                    .truncate(self.scrollback_visible_link_count);
            }
            let sb_link_n = self.scrollback_visible_link_count;
            self.paint_link_highlights(buf, link_active_style, 0..sb_link_n);
            agent::render_entry_hover(
                buf,
                layout.scrollback,
                &self.scrollback,
                self.hovered_entry,
                &theme,
            );
            if let Some(ref drag) = self.drag_selection
                && drag.anchor.entry_idx != BTW_OVERLAY_ENTRY_IDX
            {
                render_active_selection_overlay(
                    &self.last_scrollback_selection_model,
                    drag,
                    self.drag_table_geometry_for(drag.anchor.entry_idx, drag.anchor.range_id),
                    buf,
                );
            } else if let Some(ref block_drag) = self.block_drag_selection {
                render_block_drag_overlay(&self.last_scrollback_selection_model, block_drag, buf);
            } else if let Some(ref sel) = self.persistent_text_selection
                && sel.entry_idx != BTW_OVERLAY_ENTRY_IDX
            {
                render_persistent_selection_overlay(
                    &self.last_scrollback_selection_model,
                    sel,
                    self.table_geometry_for_selection(sel.entry_idx, sel.range_id),
                    buf,
                );
            }
            let any_drag_active =
                self.drag_selection.is_some() || self.block_drag_selection.is_some();
            if !any_drag_active
                && !overlay_focused
                && let Some(ref selection_box) = sb_output.selection_box
            {
                selection_box.render(buf);
                self.render_selection_buttons(
                    buf,
                    selection_box,
                    sb_output.selected_entry_area,
                    &theme,
                );
            } else {
                self.hit_sb_copy.clear();
                self.hit_sb_view.clear();
            }
            let rail_shown = self.timeline_rail.is_some();
            if !rail_shown {
                agent::render_scrollbar(
                    buf,
                    layout.scrollback,
                    layout.scrollbar_x,
                    scrollbar_cfg,
                    sb_output.scroll_info,
                    self.scrollback.is_follow_mode(),
                    &theme,
                );
            }
            if !rail_shown && scrollbar_cfg.enabled && sb_output.scroll_info.is_some() {
                self.hit_scrollbar.set(Some(Rect {
                    x: layout.scrollbar_x,
                    y: layout.scrollback.y,
                    width: 1,
                    height: layout.scrollback.height,
                }));
            } else {
                self.hit_scrollbar.clear();
            }
            if let Some(ref rail) = self.timeline_rail {
                crate::views::timeline::render_rail(buf, rail, self.timeline_hover, &theme);
                if let Some(crate::views::timeline::TimelineHit::Tick(turn_idx)) =
                    self.timeline_hover
                {
                    let preview = self
                        .timeline_hover_preview
                        .as_ref()
                        .filter(|(idx, _)| *idx == turn_idx)
                        .map(|(_, text)| text.as_str());
                    if let Some(preview) = preview {
                        crate::views::timeline::render_tick_hover_popup(
                            buf,
                            rail,
                            layout.scrollback,
                            turn_idx,
                            preview,
                            &theme,
                        );
                    }
                }
            }
        }
        let mut follow_indicator_y: Option<u16> = None;
        let mut response_top_indicator_y: Option<u16> = None;
        if self.block_viewer.is_none() && !search_active {
            use crate::appearance::FollowIndicator;
            let gap_y = layout.scrollback.y + layout.scrollback.height;
            let gap_x = layout.scrollback.x;
            let gap_w = layout.scrollback.width;
            let mut content_line_y: Option<u16> = None;
            if appearance.scrollback.display.line_under_last_entry && !self.scrollback.is_empty() {
                let (scroll_offset, _, total_height) = self.scrollback.scroll_info();
                let content_end_screen = u16::try_from(
                    layout.scrollback.y as usize + total_height.saturating_sub(scroll_offset),
                )
                .unwrap_or(u16::MAX);
                if content_end_screen <= gap_y && content_end_screen >= layout.scrollback.y {
                    let line_y = content_end_screen;
                    content_line_y = Some(line_y);
                    let line_x = gap_x + 3;
                    let line_end = (gap_x + gap_w).saturating_sub(2);
                    let line_style = ratatui::style::Style::default().fg(theme.bg_light);
                    for x in line_x..line_end {
                        if let Some(cell) = buf.cell_mut((x, line_y)) {
                            cell.set_symbol("╌");
                            cell.set_style(line_style);
                        }
                    }
                }
            }
            if appearance.scrollback.scroll.follow_indicator != FollowIndicator::None {
                if !self.scrollback.is_follow_mode()
                    && self.scrollback.has_content_below()
                    && content_line_y.is_none()
                {
                    follow_indicator_y = Some(gap_y);
                }
                if self.scrollback.has_response_top_above() {
                    response_top_indicator_y = sticky_gap_row.map(|row| layout.scrollback.y + row);
                }
            }
        }
        let indicator_center_x = layout.scrollback.x + layout.scrollback.width / 2;
        draw_scroll_arrow(
            buf,
            &theme,
            indicator_center_x,
            follow_indicator_y,
            "▼",
            &mut self.hit_follow_indicator,
        );
        draw_scroll_arrow(
            buf,
            &theme,
            indicator_center_x,
            response_top_indicator_y,
            "▲",
            &mut self.hit_response_top_indicator,
        );
        if let Some(msg) = self.active_toast_message() {
            let sb = layout.scrollback;
            if let Some(toast_text) = fit_toast_text(msg, sb.width) {
                let w = toast_text.chars().count() as u16;
                if sb.height > 0 {
                    let x = sb.right().saturating_sub(w + 1);
                    let y = sb.bottom().saturating_sub(1);
                    for (i, ch) in toast_text.chars().enumerate() {
                        if let Some(cell) = buf.cell_mut((x + i as u16, y)) {
                            cell.set_char(ch);
                            cell.fg = theme.accent_user;
                            cell.bg = theme.bg_base;
                            cell.modifier = ratatui::prelude::Modifier::BOLD;
                        }
                    }
                    self.frame_occluder_rects.push(Rect {
                        x,
                        y,
                        width: w,
                        height: 1,
                    });
                }
            }
        }
        if tasks_height > 0 {
            let bg_focused = self.active_pane == ActivePane::Tasks && !overlay_focused;
            self.tasks.render(
                layout.tasks,
                buf,
                bg_focused,
                layout_cfg,
                &self.session.bg_tasks,
                &self.subagent_sessions,
                &self.session.scheduled_tasks,
            );
            let close_rect = agent::render_todo_chrome(
                buf,
                layout.tasks,
                layout_cfg,
                bg_focused,
                false,
                self.hit_bg_close.hovered,
                &theme,
            )
            .and_then(|sel| sel.close_button_rect());
            self.hit_bg_close.set(close_rect);
        }
        if catalog_height > 0 {
            let cat_focused = self.active_pane == ActivePane::Catalog && !overlay_focused;
            self.catalog
                .render(layout.catalog, buf, cat_focused, layout_cfg);
            let close_rect = agent::render_todo_chrome(
                buf,
                layout.catalog,
                layout_cfg,
                cat_focused,
                false,
                self.hit_catalog_close.hovered,
                &theme,
            )
            .and_then(|sel| sel.close_button_rect());
            self.hit_catalog_close.set(close_rect);
        } else {
            self.hit_catalog_close.clear();
        }
        if todo_height > 0 {
            let todo_focused = self.active_pane == ActivePane::Todo && !overlay_focused;
            self.todo.render(layout.todo, buf, todo_focused, layout_cfg);
            let close_rect = agent::render_todo_chrome(
                buf,
                layout.todo,
                layout_cfg,
                todo_focused,
                false,
                self.hit_todo_close.hovered,
                &theme,
            )
            .and_then(|sel| sel.close_button_rect());
            self.hit_todo_close.set(close_rect);
        } else {
            self.hit_todo_close.clear();
        }
        if queue_height > 0 {
            let queue_focused = self.active_pane == ActivePane::Queue && !overlay_focused;
            self.queue.render(
                layout.queue,
                buf,
                queue_focused,
                layout_cfg,
                Some(layout.scrollback),
                self.can_send_now(),
            );
            let close_rect = agent::render_todo_chrome_with_close_label(
                buf,
                layout.queue,
                layout_cfg,
                queue_focused,
                false,
                self.hit_queue_close.hovered,
                &theme,
                Some(crate::glyphs::ballot_x_button()),
            )
            .and_then(|sel| sel.close_button_rect());
            self.hit_queue_close.set(close_rect);
        } else {
            self.hit_queue_close.clear();
        }
        self.dock_stop_button = None;
        if let Some(dock) = &dock_data
            && layout.dock.height > 0
        {
            crate::views::dock::render(buf, layout.dock, &theme, dock);
            self.cache_dock_stop_at(layout.dock, dock);
            let queue_body = crate::views::dock::queue_body_rect(layout.dock, dock);
            if queue_body.height > 0 {
                let queue_focused = self.active_pane == ActivePane::Queue && !overlay_focused;
                self.queue.render(
                    queue_body,
                    buf,
                    queue_focused,
                    layout_cfg,
                    Some(layout.scrollback),
                    self.can_send_now(),
                );
            }
        }
        self.last_btw_selection_model = ResolvedSelectionModel::default();
        self.last_btw_area = Rect::default();
        if btw_height > 0
            && let Some(ref btw) = self.btw_state
        {
            let tick = self.scrollback.animation_tick();
            let mut btw_links = crate::render::osc8::LinkOverlay::new();
            crate::views::btw_overlay::render_btw_panel(
                buf,
                btw,
                layout.btw,
                tick,
                self.btw_focused && self.active_pane == AgentPane::Prompt,
                Some(&mut self.hit_btw_close),
                &mut self.last_btw_selection_model,
                Some(&mut btw_links),
                &self.media_link_paths,
                self.scrollback.cwd(),
            );
            self.last_btw_area = layout.btw;
            if !btw_links.is_empty() {
                self.last_link_overlay.extend_from(&btw_links);
                self.visible_link_map.append_from_overlay(&btw_links);
            }
            let sb_n = self.scrollback_visible_link_count;
            let total_links = self.visible_link_map.len();
            self.paint_link_highlights(buf, link_active_style, sb_n..total_links);
            self.reclamp_drag_head_post_render(true);
            if let Some(width) = self
                .last_btw_selection_model
                .visible_block_content_width(BTW_OVERLAY_ENTRY_IDX)
                && self
                    .btw_selection_wrap_width
                    .is_some_and(|armed| armed != width)
            {
                self.clear_btw_owned_selection();
            }
            if let Some(ref drag) = self.drag_selection
                && drag.anchor.entry_idx == BTW_OVERLAY_ENTRY_IDX
            {
                render_active_selection_overlay(
                    &self.last_btw_selection_model,
                    drag,
                    self.drag_table_geometry_for(drag.anchor.entry_idx, drag.anchor.range_id),
                    buf,
                );
            } else if let Some(ref sel) = self.persistent_text_selection
                && sel.entry_idx == BTW_OVERLAY_ENTRY_IDX
            {
                render_persistent_selection_overlay(
                    &self.last_btw_selection_model,
                    sel,
                    self.table_geometry_for_selection(sel.entry_idx, sel.range_id),
                    buf,
                );
            }
        } else {
            self.hit_btw_close.clear();
        }
        if let Some(idx) = self.highlighted_link_idx {
            if self.visible_link_map.is_empty() {
                self.highlighted_link_idx = None;
            } else if idx >= self.visible_link_map.links().len() {
                self.highlighted_link_idx = Some(self.visible_link_map.links().len() - 1);
            }
        }
        if let Some(idx) = self.hovered_link_idx
            && (self.visible_link_map.is_empty() || idx >= self.visible_link_map.links().len())
        {
            self.hovered_link_idx = None;
        }
        if turn_status_height > 0 {
            let pad_left = if dock_on {
                crate::views::welcome::PROMPT_GUTTER
            } else {
                HorizontalLayout::ACCENT + layout_cfg.block_pad_left.saturating_sub(1)
            };
            let turn_area = Rect {
                x: layout.turn_status.x + pad_left,
                y: layout.turn_status.y,
                width: layout.turn_status.width.saturating_sub(pad_left),
                height: layout.turn_status.height,
            };
            let tick = self.scrollback.animation_tick();
            let live = self.resolve_turn_activity();
            let activity = if send_now_gap { None } else { live };
            if crate::acp::tracker::is_phase_transition(
                self.last_activity.as_ref(),
                activity.as_ref(),
            ) {
                if let Some(prev) = &self.last_activity {
                    let phase_ms = self
                        .activity_started_at
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    let prev_label = prev.as_label();
                    let next_label = activity.as_ref().map(|a| a.as_label()).unwrap_or("idle");
                    let sid = self.session.session_id.as_ref().map(|s| s.0.as_ref());
                    crate::unified_log::debug(
                        "turn.phase_transition",
                        sid,
                        Some(serde_json::json!({
                            "from": prev_label,
                            "to": next_label,
                            "phase_elapsed_ms": phase_ms,
                        })),
                    );
                }
                let hook_wait_started = matches!(
                    activity,
                    Some(crate::acp::tracker::TurnActivity::Waiting(
                        crate::acp::tracker::WaitingReason::Hooks { .. }
                    ))
                )
                .then(|| self.session.tracker.hooks_running_since())
                .flatten();
                self.activity_started_at = Some(hook_wait_started.unwrap_or_else(Instant::now));
            }
            if activity != self.last_activity {
                self.last_activity = activity.clone();
            }
            self.hit_plan_approval_status.clear();
            if let Some(ref pav) = self.plan_approval_view {
                let diamond_color = crate::views::turn_status::pending_diamond_color(
                    &theme,
                    theme.accent_plan,
                    tick,
                );
                let text_style = if self.hit_plan_approval_status.hovered {
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(ratatui::style::Modifier::UNDERLINED)
                } else {
                    Style::default().fg(theme.gray)
                };
                let status_label =
                    crate::views::plan_approval_view::plan_approval_status_label(pav.has_plan);
                let spans = vec![
                    Span::styled(
                        format!("{} ", crate::glyphs::diamond_filled()),
                        Style::default().fg(diamond_color),
                    ),
                    Span::styled(status_label, text_style),
                ];
                buf.set_line_safe(
                    turn_area.x,
                    turn_area.y,
                    &Line::from(spans),
                    turn_area.width,
                );
                let item_width: u16 = 2u16.saturating_add(status_label.len() as u16);
                self.hit_plan_approval_status.rect = Some(Rect::new(
                    turn_area.x,
                    turn_area.y,
                    item_width.min(turn_area.width),
                    1,
                ));
                self.hit_cancel_button.rect = None;
                self.hit_bg_button.rect = None;
                self.hit_watching_cue.rect = None;
            } else {
                let has_running_execute = self.surface() == ViewSurface::Root
                    && wake_display_state.is_none()
                    && self
                        .session
                        .tracker
                        .running_execute_tool_call_id()
                        .is_some();
                let is_pending_user_input = matches!(
                    self.blocking_card(),
                    Some(
                        BlockingCard::Permission
                            | BlockingCard::Question
                            | BlockingCard::McpElicitation
                    )
                );
                let goal_verifying = self
                    .goal_state
                    .as_ref()
                    .is_some_and(|g| g.verifying_completion);
                let held_queue = if dock_on { 0 } else { self.held_queue_count() };
                let held_queue_top_sendable = !dock_on && self.held_queue_top_sendable();
                let turn_output = turn_status::render_turn_status(
                    buf,
                    turn_area,
                    turn_status::TurnStatusArgs {
                        state: &status_state,
                        activity: &activity,
                        turn_elapsed: self.turn_elapsed(),
                        activity_started_at: self.activity_started_at,
                        tick,
                        drain_blocked: turn_status_drain_blocked,
                        buttons: Some(turn_status::MouseButtons {
                            cancel_hovered: self.hit_cancel_button.hovered,
                            bg_hovered: self.hit_bg_button.hovered,
                            watching_hovered: self.hit_watching_cue.hovered,
                        }),
                        has_running_execute,
                        total_tokens: self.context_state.as_ref().map(|c| c.used),
                        session_starting_since: self.session_starting_since,
                        is_bash_turn: self.bash_turn,
                        is_pending_user_input,
                        goal_verifying,
                        watchers: turn_status_watchers,
                        jev: self.session.session_id.as_ref().map_or_else(
                            || distill_shell::jev::turn_activity(self.turn_started_at),
                            |session_id| {
                                distill_shell::jev::turn_activity_for_session(
                                    session_id.0.as_ref(),
                                    self.turn_started_at,
                                )
                            },
                        ),
                        parked: turn_status_parked,
                        flat_background: false,
                        held_queue,
                        held_queue_top_sendable,
                    },
                );
                self.hit_cancel_button
                    .set_unless_dropdown(turn_output.cancel_button, dropdown_open);
                self.hit_bg_button
                    .set_unless_dropdown(turn_output.bg_button, dropdown_open);
                self.hit_watching_cue
                    .set_unless_dropdown(turn_output.watching_cue, dropdown_open);
            }
        } else {
            self.hit_cancel_button.clear();
            self.hit_bg_button.clear();
            self.hit_watching_cue.clear();
            self.hit_plan_approval_status.clear();
        }
        let privacy_banner_owns_slot =
            privacy_banner && layout.banner.height >= crate::views::privacy_banner::MIN_HEIGHT;
        if !privacy_banner_owns_slot {
            self.privacy_banner.clear_hits();
        }
        if privacy_banner_owns_slot {
            self.hit_announcement_hide.clear();
            self.hit_announcement_cta.clear();
            let rects = crate::views::privacy_banner::render(layout.banner, buf, &theme, mouse_pos);
            self.privacy_banner
                .hit_opt_in
                .set_unless_dropdown(Some(rects.opt_in), dropdown_open);
            self.privacy_banner
                .hit_opt_out
                .set_unless_dropdown(Some(rects.opt_out), dropdown_open);
            self.privacy_banner
                .hit_terms
                .set_unless_dropdown(Some(rects.terms), dropdown_open);
            self.privacy_banner
                .hit_policy
                .set_unless_dropdown(Some(rects.policy), dropdown_open);
        } else if let Some((ref msg, remaining)) = self.mode_switch_banner {
            self.hit_announcement_hide.clear();
            self.hit_announcement_cta.clear();
            if layout.banner.height > 0 && layout.banner.width > 4 {
                let bg = theme.bg_base;
                for col in 0..layout.banner.width {
                    if let Some(cell) = buf.cell_mut((layout.banner.x + col, layout.banner.y)) {
                        cell.set_char(' ');
                        cell.fg = bg;
                        cell.bg = bg;
                    }
                }
                let opacity = if remaining > MODE_BANNER_FADE_TICKS {
                    1.0
                } else {
                    remaining as f32 / MODE_BANNER_FADE_TICKS as f32
                };
                let base_fg = theme.text_secondary;
                let fg = crate::render::color::blend_color(theme.bg_base, base_fg, opacity)
                    .unwrap_or(base_fg);
                let text = format!("  {}", msg);
                let maxw = layout.banner.width.saturating_sub(2) as usize;
                let display: String = if text.len() > maxw {
                    text.chars()
                        .take(maxw.saturating_sub(1))
                        .collect::<String>()
                        + "…"
                } else {
                    text
                };
                let x = layout.banner.x;
                let y = layout.banner.y;
                for (i, ch) in display.chars().enumerate() {
                    if let Some(cell) = buf.cell_mut((x + i as u16, y)) {
                        cell.set_char(ch);
                        cell.fg = fg;
                        cell.bg = bg;
                    }
                }
            }
        } else {
            let announcement_banner_owns_slot =
                self.session_banner_active && layout.banner.height > 0;
            let banner_hits = crate::views::announcements::render_banner(
                layout.banner,
                buf,
                banner_announcements,
                hidden_announcement_ids,
                self.hit_announcement_hide.hovered,
                self.hit_announcement_cta.hovered,
                self.permission_queue.is_empty(),
            );
            self.hit_announcement_hide
                .set_unless_dropdown(banner_hits.hide, dropdown_open);
            self.hit_announcement_cta
                .set_unless_dropdown(banner_hits.cta, dropdown_open);
            if !announcement_banner_owns_slot
                && banner_height > 0
                && let Some(tip_text) = tip
            {
                crate::tips::render::render_tip(
                    layout.banner,
                    buf,
                    tip_text,
                    crate::tips::render::HINT_INSET,
                );
            }
            if !announcement_banner_owns_slot
                && tip_row_visible
                && let Some(line) = self.ephemeral_tip.line()
            {
                crate::tips::render::render_ephemeral_tip(layout.banner, buf, line);
            }
        }
        self.draw_plugin_cta(buf, layout.plugin_cta, &theme);
        if voice_listening && layout.voice_recording.height > 0 && layout.voice_recording.width > 0
        {
            let rec_area = layout.voice_recording;
            let bg = theme.bg_base;
            for col in 0..rec_area.width {
                if let Some(cell) = buf.cell_mut((rec_area.x + col, rec_area.y)) {
                    cell.set_char(' ');
                    cell.fg = bg;
                    cell.bg = bg;
                }
            }
            let content_x = rec_area.x + layout_cfg.block_pad_left;
            let (filled, brightness) = record_dot_pulse();
            let dot = crate::glyphs::record_dot(filled);
            let dot_color = crate::render::color::blend_color(bg, theme.accent_error, brightness)
                .unwrap_or(theme.accent_error);
            buf.set_string(
                content_x,
                rec_area.y,
                dot,
                Style::default().fg(dot_color).bg(bg),
            );
            buf.set_string(
                content_x + 2,
                rec_area.y,
                "Recording",
                Style::default().fg(theme.accent_error).bg(bg),
            );
            let stop_str = "[stop]";
            let stop_w = unicode_width::UnicodeWidthStr::width(stop_str) as u16;
            let stop_x = rec_area.x
                + rec_area
                    .width
                    .saturating_sub(layout_cfg.block_pad_right + stop_w);
            let stop_fg = if self.hit_voice_stop_button.hovered {
                theme.accent_error
            } else {
                theme.gray
            };
            buf.set_string(
                stop_x,
                rec_area.y,
                stop_str,
                Style::default().fg(stop_fg).bg(bg),
            );
            self.hit_voice_stop_button.rect = Some(Rect::new(stop_x, rec_area.y, stop_w, 1));
        } else {
            self.hit_voice_stop_button.clear();
        }
        self.follow_up_chips = match self.follow_ups.as_ref() {
            Some(fu) => agent::render_follow_ups(
                layout.follow_ups,
                buf,
                &theme,
                &fu.suggestions,
                self.hovered_follow_up_chip,
            ),
            None => {
                self.hovered_follow_up_chip = None;
                Vec::new()
            }
        };
        let editing_label;
        let commenting_label;
        let theme = Theme::current();
        let approval_is_commenting = self
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.focus == PlanApprovalFocus::Commenting);
        let plan_label: Option<&str> = if effective_plan || casual_commenting {
            let commenting_range: Option<&std::ops::Range<usize>> = if approval_is_commenting {
                self.plan_approval_view
                    .as_ref()
                    .and_then(|pav| pav.commenting_range.as_ref())
            } else if casual_commenting {
                self.casual_commenting_range.as_ref()
            } else {
                None
            };
            Some(if approval_is_commenting || casual_commenting {
                commenting_label = match commenting_range {
                    Some(r) if r.len() == 1 => format!("commenting L{}", r.start),
                    Some(r) => format!("commenting L{}-{}", r.start, r.end - 1),
                    None => "commenting".to_string(),
                };
                commenting_label.as_str()
            } else if self.plan_approval_view.is_some() {
                "plan approval"
            } else {
                "plan"
            })
        } else {
            self.published_mode_label()
        };
        let mut flags: Vec<PromptFlag> =
            mode_flags(plan_label, self.session.permission_label(), &theme);
        // Jev availability does not depend on the permission mode.
        if let Some(jev) = crate::views::prompt_widget::jev_flag(
            distill_shell::jev::current_status_cached(),
            &theme,
        ) {
            flags.push(jev);
        }
        let multiline = self.multiline_mode;
        let warning = self.credit_balance.as_ref().and_then(|bal| {
            crate::views::credit_bar::usage_warning_for_session(
                bal,
                self.auto_topup.as_ref(),
                self.billing_surface_visible,
                self.chat_kind,
            )
        });
        let usage_warning_text: Option<String> = warning.as_ref().map(|(t, _)| t.clone());
        let usage_warning = usage_warning_text.as_deref();
        let usage_warning_critical = warning.is_some_and(|(_, critical)| critical);
        let model_label = self.session.models.effort_label(&model_id);
        let model_tiers =
            crate::views::prompt_widget::PromptModelTiers::from_model_state(&self.session.models);
        let info = match &self.prompt_mode {
            PromptMode::Normal => PromptInfo {
                model_name: &model_label,
                flags: &flags,
                multiline,
                usage_warning,
                usage_warning_critical,
                model_tiers: model_tiers.as_ref(),
            },
            PromptMode::EditingQueued { id, .. } => {
                let pos = self.session.queue_position(*id).map(|i| i + 1).unwrap_or(1);
                editing_label = format!("editing queued #{pos}");
                PromptInfo {
                    model_name: &editing_label,
                    flags: &flags,
                    multiline,
                    usage_warning,
                    usage_warning_critical,
                    model_tiers: None,
                }
            }
        };
        let info = if let Some(label) = self.prompt_input_mode.prompt_info_override() {
            PromptInfo {
                model_name: label,
                flags: &[],
                multiline: false,
                usage_warning,
                usage_warning_critical,
                model_tiers: None,
            }
        } else {
            info
        };
        let mut prompt_cursor_pos: Option<(u16, u16)> = None;
        let mut prompt_post_flush: Option<crate::terminal::overlay::PostFlush> = None;
        if permission_view_h > 0 {
            let perm_area = layout.prompt;
            if let Some(perm) = self.permission_queue.front() {
                let followup_text = self.prompt.text();
                let render_result = crate::views::permission_view::render_permission_view(
                    buf,
                    perm_area,
                    perm,
                    followup_text,
                    self.permission_pattern_edit.as_ref(),
                    self.hovered_permission_item,
                    &theme,
                    prompt_focused,
                );
                if let Some(ref iarea) = render_result.inline_prompt {
                    let row_bg = theme.bg_visual;
                    let remaining_h = (perm_area.y + perm_area.height).saturating_sub(iarea.y);
                    let perm_followup_style = PromptStyle {
                        focused: true,
                        show_prefix: false,
                        vpad_top: 0,
                        chrome: false,
                        chrome_pad_left: 0,
                        chrome_pad_right: 0,
                        bg: PromptBg::Panel(row_bg),
                        accent_color_override: None,
                        border_color_override: None,
                        prefix_override: None,
                        placeholder_when_focused: false,
                        placeholder_override: None,
                        compact: false,
                        show_accent_line: false,
                        show_borders: false,
                        title: None,
                        image_preview: !self.resize_hides_prompt_preview(),
                    };
                    let prompt_h = remaining_h.saturating_sub(1).max(1);
                    let prompt_draw_area = Rect {
                        x: iarea.text_x,
                        y: iarea.y,
                        width: iarea.text_w,
                        height: prompt_h,
                    };
                    let prompt_result_inner = self.prompt.draw(
                        buf,
                        prompt_draw_area,
                        None,
                        &perm_followup_style,
                        None,
                        None,
                    );
                    if let Some(pos) = prompt_result_inner.cursor_pos {
                        prompt_cursor_pos = Some(pos);
                    }
                    let accent_color = theme.accent_user;
                    let track_x = prompt_draw_area.x + prompt_draw_area.width.saturating_sub(1);
                    let accent_end = iarea.y.saturating_add(prompt_h);
                    for extra_y in (iarea.y + 1)..accent_end {
                        buf.set_style(
                            Rect {
                                x: layout.prompt.x + 1,
                                y: extra_y,
                                width: track_x.saturating_sub(layout.prompt.x + 1),
                                height: 1,
                            },
                            Style::default().bg(row_bg),
                        );
                        if let Some(cell) = buf.cell_mut((layout.prompt.x, extra_y)) {
                            cell.set_symbol(crate::glyphs::accent_bar());
                            cell.set_style(Style::default().fg(accent_color).bg(row_bg));
                        }
                    }
                    let track_bg = Style::default().bg(theme.bg_dark);
                    for track_y in iarea.y..accent_end {
                        if let Some(cell) = buf.cell_mut((track_x, track_y))
                            && cell.symbol() == " "
                        {
                            cell.set_style(track_bg);
                        }
                    }
                }
            }
        } else if question_view_h > 0 {
            let question_accent = if effective_plan {
                theme.accent_plan
            } else {
                theme.accent_user
            };
            let is_input_mode = self
                .question_view
                .as_ref()
                .map(|qv| qv.focus == crate::views::question_view::QuestionFocus::InputMode)
                .unwrap_or(false);
            let inline_prompt_h: u16 = if is_input_mode {
                question_prompt_body_h.max(1)
            } else {
                0
            };
            let question_area = Rect {
                x: layout.prompt.x,
                y: layout.prompt.y,
                width: layout.prompt.width,
                height: layout
                    .prompt
                    .height
                    .saturating_sub(inline_prompt_h)
                    .saturating_sub(question_footer_h),
            };
            if let Some(ref mut qv) = self.question_view {
                let content_w = inner_width.saturating_sub(QUESTION_VIEW_HPAD) as usize;
                if let Some(question) = qv.questions.get(qv.active_tab) {
                    let visible_options_h = crate::views::question_view::visible_options_height(
                        question,
                        question_area.height,
                        content_w,
                        qv.focused_preview(),
                        qv.fullscreen,
                        qv.cached_desc_cap,
                        qv.cached_preview_cap,
                    );
                    qv.clamp_scroll(visible_options_h, content_w);
                }
            }
            if let Some(ref qv) = self.question_view {
                let render_result = crate::views::question_view::render_question_view(
                    buf,
                    question_area,
                    qv,
                    self.hovered_question_item,
                    &theme,
                    prompt_focused,
                );
                self.question_scroll_region =
                    Some((render_result.options_start_y, render_result.options_end_y));
            }
            if is_input_mode && inline_prompt_h > 0 {
                let row_y = question_area.y + question_area.height;
                let content_x = layout.prompt.x + 3;
                let content_w = layout.prompt.width.saturating_sub(3);
                let row_bg = theme.bg_visual;
                buf.set_style(
                    Rect {
                        x: content_x,
                        y: row_y,
                        width: content_w,
                        height: 1,
                    },
                    Style::default().bg(row_bg),
                );
                let prefix_w = self
                    .question_view
                    .as_ref()
                    .and_then(|qv| qv.questions.get(qv.active_tab))
                    .map(crate::views::question_view::option_prefix_w)
                    .unwrap_or(6) as u16;
                let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
                let marker_style = Style::default()
                    .fg(theme.text_primary)
                    .bg(row_bg)
                    .add_modifier(ratatui::style::Modifier::BOLD);
                let prompt_ind = Style::default().fg(theme.accent_user).bg(row_bg);
                buf.set_span_safe(content_x, row_y, &Span::styled("z ", num_style), 2);
                let is_multi = self
                    .question_view
                    .as_ref()
                    .and_then(|qv| qv.questions.get(qv.active_tab))
                    .and_then(|q| q.multi_select)
                    .unwrap_or(false);
                let freeform_sel = self
                    .question_view
                    .as_ref()
                    .and_then(|qv| {
                        qv.per_question_freeform_selected
                            .get(qv.active_tab)
                            .copied()
                    })
                    .unwrap_or(false);
                let (marker_text, actual_marker_style) = if is_multi {
                    if freeform_sel {
                        ("[x] ".to_string(), marker_style)
                    } else {
                        (
                            "[ ] ".to_string(),
                            Style::default().fg(theme.gray).bg(row_bg),
                        )
                    }
                } else if freeform_sel {
                    (format!("({}) ", crate::glyphs::filled_dot()), marker_style)
                } else {
                    (
                        "(\u{25cb}) ".to_string(),
                        Style::default().fg(theme.gray).bg(row_bg),
                    )
                };
                buf.set_span_safe(
                    content_x + 2,
                    row_y,
                    &Span::styled(marker_text, actual_marker_style),
                    4,
                );
                buf.set_span_safe(
                    content_x + prefix_w,
                    row_y,
                    &Span::styled(crate::glyphs::prompt_arrow(), prompt_ind),
                    2,
                );
                let text_x = content_x + prefix_w + 2;
                let text_w = content_w.saturating_sub(prefix_w + 2);
                let text_style = PromptStyle {
                    show_prefix: false,
                    ..question_input_style.clone()
                };
                let prompt_draw_area = Rect {
                    x: text_x,
                    y: row_y,
                    width: text_w,
                    height: inline_prompt_h,
                };
                let prompt_result_inner = self.prompt.draw(
                    buf,
                    prompt_draw_area,
                    Some(layout.scrollback),
                    &text_style,
                    None,
                    None,
                );
                prompt_cursor_pos = prompt_result_inner.cursor_pos;
                self.inline_prompt_area = Some(Rect {
                    x: layout.prompt.x,
                    y: row_y,
                    width: layout.prompt.width,
                    height: inline_prompt_h,
                });
                for y in row_y..row_y.saturating_add(inline_prompt_h) {
                    if y > row_y {
                        buf.set_style(
                            Rect {
                                x: content_x,
                                y,
                                width: prefix_w + 2,
                                height: 1,
                            },
                            Style::default().bg(row_bg),
                        );
                    }
                    for col in (layout.prompt.x + 1)..(layout.prompt.x + 3) {
                        if let Some(cell) = buf.cell_mut((col, y)) {
                            cell.set_char(' ');
                            cell.set_style(Style::default().bg(row_bg));
                        }
                    }
                    if let Some(cell) = buf.cell_mut((layout.prompt.x, y)) {
                        cell.set_symbol(crate::glyphs::accent_bar());
                        cell.set_style(Style::default().fg(question_accent).bg(row_bg));
                    }
                }
            }
            if !is_input_mode {
                self.inline_prompt_area = None;
            }
            if let Some(ref qv) = self.question_view {
                let footer_y = question_area.y + question_area.height + inline_prompt_h + 1;
                let footer_x = layout.prompt.x;
                let footer_w = layout.prompt.width;
                self.question_nav_buttons.clear();
                if footer_y < layout.prompt.y + layout.prompt.height && footer_w > 10 {
                    use ratatui::style::Modifier;
                    let footer_bg = theme.bg_light;
                    let gap_above = footer_y.saturating_sub(1);
                    if gap_above >= question_area.y + question_area.height + inline_prompt_h {
                        buf.set_style(
                            Rect {
                                x: footer_x,
                                y: gap_above,
                                width: footer_w,
                                height: 1,
                            },
                            Style::default().bg(footer_bg),
                        );
                    }
                    let gap_below = footer_y + 1;
                    if gap_below < layout.prompt.y + layout.prompt.height {
                        buf.set_style(
                            Rect {
                                x: footer_x,
                                y: gap_below,
                                width: footer_w,
                                height: 1,
                            },
                            Style::default().bg(footer_bg),
                        );
                    }
                    let footer_rect = Rect {
                        x: footer_x,
                        y: footer_y,
                        width: footer_w,
                        height: 1,
                    };
                    buf.set_style(footer_rect, Style::default().bg(footer_bg));
                    let content_x = layout.prompt.x + 3;
                    let hint_style = Style::default()
                        .fg(theme.gray)
                        .bg(footer_bg)
                        .add_modifier(Modifier::BOLD);
                    let hint_key = Style::default()
                        .fg(question_accent)
                        .bg(footer_bg)
                        .add_modifier(Modifier::BOLD);
                    let left_spans = Self::answer_footer_hints(qv, hint_style, hint_key);
                    let left_line = Line::from(left_spans);
                    let avail_w = footer_w.saturating_sub(3);
                    buf.set_line_safe(content_x, footer_y, &left_line, avail_w);
                    let is_last = qv.active_tab >= qv.questions.len().saturating_sub(1);
                    let enter_label = if qv.is_on_freeform_row() {
                        "edit"
                    } else if is_last {
                        "submit"
                    } else {
                        "select"
                    };
                    let btn_key = "Enter";
                    let btn_bg = theme.bg_base;
                    let bkey_style = Style::default()
                        .fg(question_accent)
                        .bg(btn_bg)
                        .add_modifier(Modifier::BOLD);
                    let blabel_style = Style::default().fg(theme.gray).bg(btn_bg);
                    let bpad_style = Style::default().bg(btn_bg);
                    let bw = (1 + btn_key.len() + 1 + enter_label.len() + 1) as u16;
                    let btn_x = footer_x + footer_w.saturating_sub(3).saturating_sub(bw);
                    if btn_x > content_x {
                        buf.set_span_safe(btn_x, footer_y, &Span::styled(" ", bpad_style), 1);
                        buf.set_span_safe(
                            btn_x + 1,
                            footer_y,
                            &Span::styled(btn_key, bkey_style),
                            btn_key.len() as u16,
                        );
                        let cx = btn_x + 1 + btn_key.len() as u16;
                        buf.set_span_safe(cx, footer_y, &Span::styled(":", blabel_style), 1);
                        buf.set_span_safe(
                            cx + 1,
                            footer_y,
                            &Span::styled(enter_label, blabel_style),
                            enter_label.len() as u16,
                        );
                        buf.set_span_safe(
                            cx + 1 + enter_label.len() as u16,
                            footer_y,
                            &Span::styled(" ", bpad_style),
                            1,
                        );
                        let btn_rect = Rect {
                            x: btn_x,
                            y: footer_y,
                            width: bw,
                            height: 1,
                        };
                        self.question_nav_buttons.push(('\n', btn_rect));
                    }
                }
            }
            let accent_style = Style::default().fg(question_accent).bg(theme.bg_light);
            for y in layout.prompt.y..layout.prompt.y + layout.prompt.height {
                if let Some(cell) = buf.cell_mut((layout.prompt.x, y)) {
                    cell.set_symbol(crate::glyphs::accent_bar());
                    cell.set_style(accent_style);
                }
            }
            if let Some(ref qv) = self.question_view {
                let inner_scrollbar_x = layout.prompt.x + layout.prompt.width - 1;
                let scroll_region = self
                    .question_scroll_region
                    .unwrap_or((question_area.y, question_area.y + question_area.height));
                let sb_rect = crate::views::question_view::render_question_scrollbar(
                    buf,
                    inner_scrollbar_x,
                    qv,
                    &theme,
                    scroll_region,
                );
                self.hit_question_scrollbar.set(sb_rect);
            } else {
                self.hit_question_scrollbar.clear();
            }
        } else if elicitation_view_h > 0 {
            self.elicit_hits.clear();
            if let Some(ev) = self.elicitation_view.as_mut() {
                let elicit_area = Rect {
                    x: layout.prompt.x,
                    y: layout.prompt.y,
                    width: layout.prompt.width,
                    height: layout.prompt.height,
                };
                crate::views::elicitation_view::render_elicitation_view(
                    buf,
                    elicit_area,
                    ev,
                    &theme,
                    prompt_focused,
                    Some(&mut self.elicit_hits),
                );
            }
        } else if rewind_view_h > 0 {
            if let Some(ref rw) = self.rewind_state {
                crate::views::rewind::render_rewind_overlay(
                    buf,
                    layout.prompt,
                    &rw.phase,
                    prompt_focused,
                );
            }
        } else if jump_view_h > 0 {
            if let Some(ref js) = self.jump_state {
                crate::views::jump::render_jump_overlay(buf, layout.prompt, js, prompt_focused);
            }
        } else if cancel_turn_view_h > 0 {
            let buttons = &mut self.cancel_turn_buttons;
            if let Some(ctv) = self.cancel_turn_view.as_ref() {
                modal::render_cancel_turn_panel(buf, layout.prompt, ctv, prompt_focused, buttons);
            } else {
                buttons.clear();
            }
        } else {
            let collapsed = !prompt_focused && appearance.prompt.collapse_unfocused;
            let saved_scroll = if collapsed {
                let s = self.prompt.scroll();
                let ovr = self.prompt.textarea.scroll_override();
                self.prompt.textarea.set_scroll_override(Some(0));
                Some((s, ovr))
            } else {
                None
            };
            let voice_overlay = if voice_available && (voice_listening || voice_interim.is_some()) {
                Some(crate::views::prompt_widget::VoicePromptOverlay {
                    interim: voice_interim,
                    color: theme.accent_running,
                })
            } else {
                None
            };
            let prompt_result_inner = self.prompt.draw(
                buf,
                layout.prompt,
                Some(layout.scrollback),
                &prompt_style,
                Some(&info),
                voice_overlay,
            );
            if let Some((s, ovr)) = saved_scroll {
                self.prompt.textarea.set_scroll_override(ovr);
                self.prompt.set_scroll(s);
            }
            prompt_cursor_pos = prompt_result_inner.cursor_pos;
            self.hit_model_tier_change.rect = prompt_result_inner.change_rect;
            if let Some(escapes) = prompt_result_inner.post_flush_escapes {
                prompt_post_flush = Some(escapes.into());
            }
        }
        if self.prompt.file_search_visible() {
            use crate::views::file_search::dropdown::{MAX_DROPDOWN_ROWS, render_dropdown};
            let item_count = self.prompt.file_search.result_count();
            let item_rows = (item_count as u16).min(MAX_DROPDOWN_ROWS);
            if item_rows > 0 {
                self.prompt.file_search.ensure_visible(item_rows as usize);
                let bottom_border_y = if let Some(ipa) = self.inline_prompt_area {
                    ipa.y.saturating_sub(1)
                } else {
                    layout.prompt.y.saturating_sub(1)
                };
                let panel_height = item_rows + 2;
                let top_border_y = bottom_border_y.saturating_sub(panel_height - 1);
                let panel_x = area.x + layout_cfg.eff_hpad_left(compact);
                let panel_width = area.width.saturating_sub(
                    layout_cfg.eff_hpad_left(compact) + layout_cfg.eff_hpad_right(compact),
                );
                if top_border_y < bottom_border_y && panel_width > 4 {
                    let panel_area = Rect {
                        x: panel_x,
                        y: top_border_y,
                        width: panel_width,
                        height: panel_height,
                    };
                    ratatui::widgets::Clear.render(panel_area, buf);
                    self.frame_occluder_rects.push(panel_area);
                    buf.set_style(
                        panel_area,
                        Style::default().fg(theme.text_primary).bg(theme.bg_light),
                    );
                    let border_style = Style::default()
                        .fg(theme.panel_border_fg())
                        .bg(theme.bg_base);
                    let border_line = Line::styled("─".repeat(panel_width as usize), border_style);
                    buf.set_line_safe(panel_x, top_border_y, &border_line, panel_width);
                    buf.set_line_safe(panel_x, bottom_border_y, &border_line, panel_width);
                    {
                        let (k, n) = (
                            self.prompt.file_search.result_count(),
                            self.prompt.file_search.total_items(),
                        );
                        let hint = if k >= 1000 {
                            format!("1k+/{n}")
                        } else {
                            format!("{k}/{n}")
                        };
                        let hint_w = hint.len() as u16;
                        if hint_w + 2 <= panel_width {
                            let hint_x = panel_x + panel_width - hint_w - 1;
                            let hint_line = Line::styled(
                                hint,
                                Style::default().fg(theme.gray).bg(theme.bg_base),
                            );
                            buf.set_line_safe(hint_x, top_border_y, &hint_line, hint_w);
                        }
                    }
                    let items_x = layout.prompt.x + dropdown_content_inset();
                    let items_width = dropdown_items_width(layout.prompt);
                    let items_area = Rect {
                        x: items_x,
                        y: top_border_y + 1,
                        width: items_width,
                        height: item_rows,
                    };
                    render_dropdown(buf, items_area, &self.prompt.file_search, &theme);
                    self.dropdown_items_area = Some(items_area);
                } else {
                    self.dropdown_items_area = None;
                }
            } else {
                self.dropdown_items_area = None;
            }
        } else {
            self.dropdown_items_area = None;
        }
        if !self.prompt.file_search_visible() && self.prompt.slash_open() {
            use crate::views::slash_dropdown::{
                desired_item_rows, render_dropdown as render_slash,
            };
            let snap = self.prompt.slash_snapshot();
            let item_count = snap.matches.len();
            let items_width = dropdown_items_width(layout.prompt);
            let item_rows = desired_item_rows(&snap.matches, items_width);
            if item_rows > 0 {
                if let Some(chrome) = render_dropdown_chrome(
                    buf,
                    item_count,
                    item_rows,
                    self.inline_prompt_area,
                    layout.prompt,
                    area,
                    layout_cfg,
                    compact,
                    false,
                    &theme,
                ) {
                    let hovered = self.prompt.slash_hovered();
                    self.slash_dropdown_hit =
                        render_slash(buf, chrome.items, &snap, hovered, &theme);
                    self.slash_dropdown_items_area = Some(chrome.items);
                    self.frame_occluder_rects.push(chrome.panel);
                } else {
                    self.slash_dropdown_items_area = None;
                    self.slash_dropdown_hit = Default::default();
                }
            } else {
                self.slash_dropdown_items_area = None;
                self.slash_dropdown_hit = Default::default();
            }
        } else {
            self.slash_dropdown_items_area = None;
            self.slash_dropdown_hit = Default::default();
        }
        if !self.prompt.slash_open()
            && !self.prompt.file_search_visible()
            && self.prompt.completion_dropdown_open()
        {
            use crate::views::completion_dropdown::{
                MAX_VISIBLE_ROWS, render_dropdown as render_completions,
            };
            let dd = &self.prompt.suggestions.dropdown;
            let item_count = dd.items.len();
            let item_rows = (item_count as u16).min(MAX_VISIBLE_ROWS);
            if item_rows > 0 {
                if let Some(chrome) = render_dropdown_chrome(
                    buf,
                    item_count,
                    item_rows,
                    self.inline_prompt_area,
                    layout.prompt,
                    area,
                    layout_cfg,
                    compact,
                    false,
                    &theme,
                ) {
                    render_completions(buf, chrome.items, dd, &theme);
                    self.completion_dropdown_items_area = Some(chrome.items);
                    self.frame_occluder_rects.push(chrome.panel);
                } else {
                    self.completion_dropdown_items_area = None;
                }
            } else {
                self.completion_dropdown_items_area = None;
            }
        } else {
            self.completion_dropdown_items_area = None;
        }
        if self.prompt.history_search.is_active() {
            use crate::render::scrollbar::render_scrollbar_styled;
            use ratatui::style::Modifier;
            let max_rows: u16 = 8;
            let result_count = self.prompt.history_search.result_count();
            let item_rows = (result_count as u16).min(max_rows).max(1);
            let panel_height = item_rows + 2;
            let bottom_border_y = layout.prompt.y.saturating_sub(1);
            let top_border_y = bottom_border_y.saturating_sub(panel_height - 1);
            let panel_x = area.x + layout_cfg.eff_hpad_left(compact);
            let panel_width = area.width.saturating_sub(
                layout_cfg.eff_hpad_left(compact) + layout_cfg.eff_hpad_right(compact),
            );
            if panel_height >= 3 && top_border_y < bottom_border_y && panel_width > 4 {
                let panel_area = Rect {
                    x: panel_x,
                    y: top_border_y,
                    width: panel_width,
                    height: panel_height,
                };
                ratatui::widgets::Clear.render(panel_area, buf);
                self.frame_occluder_rects.push(panel_area);
                buf.set_style(
                    panel_area,
                    Style::default().fg(theme.text_primary).bg(theme.bg_light),
                );
                let border_style = Style::default()
                    .fg(theme.panel_border_fg())
                    .bg(theme.bg_base);
                let border_line =
                    Line::styled("\u{2500}".repeat(panel_width as usize), border_style);
                buf.set_line_safe(panel_x, top_border_y, &border_line, panel_width);
                buf.set_line_safe(panel_x, bottom_border_y, &border_line, panel_width);
                let caption_style = theme.muted_over_chrome();
                {
                    let hint = format!("{result_count}");
                    let hint_w = hint.len() as u16;
                    if hint_w + 2 <= panel_width {
                        let hint_x = panel_x + panel_width - hint_w - 1;
                        buf.set_line_safe(
                            hint_x,
                            top_border_y,
                            &Line::styled(hint, caption_style.bg(theme.bg_base)),
                            hint_w,
                        );
                    }
                    let label = " history ";
                    let label_w = label.len() as u16;
                    if label_w + 2 <= panel_width {
                        buf.set_line_safe(
                            panel_x + 1,
                            top_border_y,
                            &Line::styled(label, caption_style.bg(theme.bg_base)),
                            label_w,
                        );
                    }
                }
                let items_x = layout.prompt.x + dropdown_content_inset();
                let items_width = dropdown_items_width(layout.prompt);
                let items_area_rect = Rect {
                    x: items_x,
                    y: top_border_y + 1,
                    width: items_width,
                    height: item_rows,
                };
                self.history_dropdown_area = Some(items_area_rect);
                let hover_bg = theme.bg_hover;
                if result_count == 0 {
                    let msg_y = top_border_y + 1;
                    if msg_y < bottom_border_y {
                        let message = if self.prompt_history_loading() {
                            "  Loading..."
                        } else {
                            "  no matching history"
                        };
                        buf.set_string_safe(
                            items_x,
                            msg_y,
                            message,
                            caption_style.bg(theme.bg_light),
                        );
                    }
                } else {
                    let selected = self.prompt.history_search.selected;
                    let visible_rows = item_rows as usize;
                    let scroll_offset = if selected >= visible_rows {
                        selected - (visible_rows - 1)
                    } else {
                        0
                    };
                    let visible_end = result_count.min(scroll_offset + visible_rows);
                    let needs_scrollbar = result_count > visible_rows;
                    let text_width = if needs_scrollbar {
                        items_width.saturating_sub(2)
                    } else {
                        items_width
                    };
                    for (vi, ri) in (scroll_offset..visible_end).enumerate() {
                        let row_y = top_border_y + 1 + vi as u16;
                        if row_y >= bottom_border_y {
                            break;
                        }
                        let result = match self.prompt.history_search.result_at(ri) {
                            Some(r) => r,
                            None => continue,
                        };
                        let is_selected = ri == selected;
                        let is_hovered =
                            self.prompt.history_search.hovered() == Some(ri) && !is_selected;
                        let row_bg = if is_selected {
                            theme.bg_visual
                        } else if is_hovered {
                            hover_bg
                        } else {
                            theme.bg_light
                        };
                        let bold = if is_selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        };
                        let fill_width = if needs_scrollbar {
                            items_width.saturating_sub(1)
                        } else {
                            items_width
                        };
                        for col in items_x..items_x + fill_width {
                            if let Some(cell) = buf.cell_mut((col, row_y)) {
                                cell.set_char(' ');
                                cell.set_style(Style::default().bg(row_bg));
                            }
                        }
                        let prefix_w: u16 = crate::glyphs::PROMPT_ARROW_WIDTH;
                        let prefix = if is_selected {
                            crate::glyphs::prompt_arrow()
                        } else {
                            "  "
                        };
                        let pfx_style = Style::default()
                            .fg(theme.text_primary)
                            .bg(row_bg)
                            .add_modifier(bold);
                        for (i, ch) in prefix.chars().enumerate() {
                            let px = items_x + i as u16;
                            if px < items_x + text_width
                                && let Some(cell) = buf.cell_mut((px, row_y))
                            {
                                cell.set_char(ch);
                                cell.set_style(if is_selected {
                                    pfx_style
                                } else {
                                    Style::default().bg(row_bg)
                                });
                            }
                        }
                        let match_style = Style::default()
                            .fg(theme.accent_user)
                            .bg(row_bg)
                            .add_modifier(bold);
                        let normal_style = Style::default()
                            .fg(theme.text_primary)
                            .bg(row_bg)
                            .add_modifier(bold);
                        let mut indices = result.indices.as_slice();
                        let mut col = items_x + prefix_w;
                        let max_col = items_x + prefix_w + text_width.saturating_sub(prefix_w);
                        let preview: String = result
                            .text
                            .chars()
                            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                            .collect();
                        for (ci, ch) in preview.chars().enumerate() {
                            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                            if col + cw > max_col {
                                if col > items_x + prefix_w
                                    && let Some(cell) = buf.cell_mut((col.saturating_sub(1), row_y))
                                {
                                    cell.set_char('\u{2026}');
                                }
                                break;
                            }
                            let is_match = indices.first() == Some(&(ci as u32));
                            if is_match {
                                let Some(rest) = indices.get(1..) else { break };
                                indices = rest;
                            }
                            let st = if is_match { match_style } else { normal_style };
                            if let Some(cell) = buf.cell_mut((col, row_y)) {
                                cell.set_symbol(&ch.to_string());
                                cell.set_style(st);
                            }
                            col += cw;
                        }
                        let row_rect = Rect {
                            x: items_x,
                            y: row_y,
                            width: fill_width,
                            height: 1,
                        };
                        if is_selected {
                            buf.set_style(row_rect, theme.selection_overlay());
                        } else if is_hovered {
                            buf.set_style(row_rect, theme.hover_overlay());
                        }
                    }
                    if needs_scrollbar {
                        let sb_area = Rect {
                            x: items_x + items_width - 1,
                            y: top_border_y + 1,
                            width: 1,
                            height: item_rows,
                        };
                        render_scrollbar_styled(
                            buf,
                            Some(sb_area),
                            result_count as u16,
                            item_rows,
                            scroll_offset as u16,
                            Style::default().bg(theme.bg_dark),
                            Style::default().fg(theme.gray_dim).bg(theme.bg_dark),
                        );
                    }
                }
            }
        } else {
            self.history_dropdown_area = None;
        }
        if self.active_modal.is_some() {
            self.draw_active_modal(area, buf, theme, compact);
            self.pane_areas = layout.pane_areas();
            return (None, crate::terminal::overlay::clear().map(Into::into));
        }
        match self.shortcuts_bar_content(registry) {
            ShortcutsBarContent::Hidden => {}
            ShortcutsBarContent::Surface(hints) => {
                ShortcutsBar::new(&hints)
                    .with_pending(pending_hint)
                    .render(layout.shortcuts, buf);
            }
            ShortcutsBarContent::Pane(hints) => {
                let help_hint = registry.find(ActionId::ShortcutsHelp).map(|def| {
                    let mut hint = def.hint();
                    if in_dashboard_overlay
                        && def.default_key == key!('x', CONTROL)
                        && let Some(alt) = def.alt_keys.first()
                    {
                        hint.keys = vec![*alt];
                    }
                    hint
                });
                ShortcutsBar::new(&hints)
                    .compact(5, help_hint)
                    .with_pending(pending_hint)
                    .render(layout.shortcuts, buf);
            }
        }
        let line_viewer_toast = self.active_toast_message().map(|s| s.to_string());
        let is_plan_viewer = self.is_plan_viewer();
        let has_plan_comments = !self.plan_comments.is_empty();
        let casual_commenting = self.is_casual_commenting();
        if self.line_viewer.is_some() {
            use crate::views::file_search::line_viewer::render_line_viewer;
            use crate::views::shortcuts_bar::HintItem;
            let plan_prompt_focused = self
                .plan_approval_view
                .as_ref()
                .is_some_and(|p| p.focus != PlanApprovalFocus::Preview);
            let overlay_bottom = if layout.turn_status.height > 0 {
                layout.turn_status.y
            } else if layout.voice_recording.height > 0 {
                layout.voice_recording.y
            } else {
                layout.prompt.y
            };
            let overlay_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: overlay_bottom.saturating_sub(area.y),
            };
            let approval_comment_count = self
                .plan_approval_view
                .as_ref()
                .map(|pav| pav.comments.len())
                .unwrap_or(0);
            let effective_comment_count = if approval_comment_count > 0 {
                approval_comment_count
            } else {
                self.plan_comments.len()
            };
            let mermaid_placements = self
                .line_viewer
                .as_mut()
                .map(|viewer| {
                    if let Some(ref pav) = self.plan_approval_view {
                        viewer.plan_mut().active_commenting_range = pav.commenting_range.clone();
                    } else {
                        viewer.plan_mut().active_commenting_range =
                            self.casual_commenting_range.clone();
                    }
                    render_line_viewer(
                        buf,
                        overlay_area,
                        viewer,
                        &self.session.cwd,
                        &theme,
                        effective_comment_count,
                    );
                    viewer
                        .last_popup_area
                        .map(|area| viewer.diagram_affordance_placements(area))
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            self.inline_media_hits = super::InlineMediaHitAreas::default();
            self.paint_diagram_affordances(buf, mermaid_placements, &theme);
            let Some(viewer) = self.line_viewer.as_mut() else {
                return (prompt_cursor_pos, prompt_post_flush);
            };
            let toast_area = viewer
                .last_popup_area
                .or(viewer.last_modal_area)
                .unwrap_or(overlay_area);
            if let Some(ref msg) = line_viewer_toast
                && toast_area.height > 0
                && let Some(toast_text) = fit_toast_text(msg, toast_area.width.saturating_sub(1))
            {
                let w = toast_text.chars().count() as u16;
                let tx = toast_area.right().saturating_sub(w + 1);
                let ty = toast_area.bottom().saturating_sub(1);
                for (i, ch) in toast_text.chars().enumerate() {
                    if let Some(cell) = buf.cell_mut((tx + i as u16, ty)) {
                        cell.set_char(ch);
                        cell.fg = theme.accent_user;
                        cell.bg = theme.bg_base;
                        cell.modifier = ratatui::prelude::Modifier::BOLD;
                    }
                }
            }
            let in_plan_approval = self.plan_approval_view.is_some();
            let on_comment = in_plan_approval
                && viewer
                    .list_state
                    .selected_index()
                    .and_then(|vi| {
                        let pi = viewer.list_state.to_physical(vi);
                        viewer.lines.get(pi)
                    })
                    .is_some_and(|item| item.comment_id().is_some());
            let approval_has_comments = in_plan_approval
                && self
                    .plan_approval_view
                    .as_ref()
                    .is_some_and(|pav| !pav.comments.is_empty());
            let viewer_hints = if in_plan_approval && on_comment {
                let mut h = vec![
                    HintItem::new(key!(Enter), "edit"),
                    HintItem::new(key!('x'), "delete"),
                ];
                if approval_has_comments {
                    h.push(HintItem::new(key!('s'), "send"));
                } else {
                    h.push(HintItem::new(key!('a'), "approve"));
                }
                h.push(HintItem::new(key!('y'), "copy plan"));
                h.push(HintItem::new(key!('q'), "quit plan"));
                h.push(HintItem::new(key!(Tab), "prompt"));
                h
            } else if in_plan_approval {
                let mut h = vec![
                    HintItem::new(key!('c'), "comment"),
                    HintItem::new(key!('y'), "copy plan"),
                ];
                if approval_has_comments {
                    h.push(HintItem::new(key!('s'), "send"));
                } else {
                    h.push(HintItem::new(key!('a'), "approve"));
                }
                h.push(HintItem::new(key!('q'), "quit plan"));
                if self.vim_mode {
                    h.push(HintItem::paired(key!('j'), key!('k'), "nav"));
                }
                h.push(HintItem::new(key!('v'), "select"));
                h.push(HintItem::new(key!(Tab), "prompt"));
                h
            } else if is_plan_viewer {
                let on_casual_comment = viewer
                    .list_state
                    .selected_index()
                    .and_then(|vi| {
                        let pi = viewer.list_state.to_physical(vi);
                        viewer.lines.get(pi)
                    })
                    .is_some_and(|item| item.comment_id().is_some());
                let mut h = if on_casual_comment {
                    vec![
                        HintItem::new(key!(Enter), "edit"),
                        HintItem::new(key!('x'), "delete"),
                        HintItem::new(key!('y'), "copy plan"),
                    ]
                } else {
                    vec![
                        HintItem::new(key!('c'), "comment"),
                        HintItem::new(key!('y'), "copy plan"),
                    ]
                };
                if has_plan_comments {
                    h.push(HintItem::new(key!('s'), "send"));
                }
                if self.vim_mode {
                    h.push(HintItem::paired(key!('j'), key!('k'), "nav"));
                }
                h.push(HintItem::new(key!('v'), "select"));
                h.push(HintItem::new(key!('f', CONTROL), "fullscreen"));
                h.push(HintItem::new(key!('/'), "search"));
                h.push(HintItem::new(key!(Esc), "close"));
                h
            } else {
                let mut h = vec![HintItem::new(key!(Enter), "confirm")];
                if self.vim_mode {
                    h.push(HintItem::paired(key!('j'), key!('k'), "nav"));
                }
                h.push(HintItem::new(key!('v'), "select"));
                h.push(HintItem::new(key!('x'), "clear"));
                if self.vim_mode {
                    h.push(HintItem::new(key!('y'), "copy"));
                    h.push(HintItem::new(key!('Y'), "filename"));
                }
                h.push(HintItem::new(key!(':'), "goto"));
                h.push(HintItem::new(key!('/'), "search"));
                h.push(HintItem::new(key!(Esc), "cancel"));
                h
            };
            let input_bar_active = viewer.list_state.input_mode().is_some();
            if !(plan_prompt_focused || casual_commenting || viewer.fullscreen && input_bar_active)
            {
                ShortcutsBar::new(&viewer_hints).render(layout.shortcuts, buf);
            }
            self.pane_areas = layout.pane_areas();
            let viewer_cursor = if plan_prompt_focused || self.is_casual_commenting() {
                prompt_cursor_pos
            } else {
                None
            };
            return (viewer_cursor, prompt_post_flush);
        }
        if let Some(ref mut viewer) = self.image_viewer {
            use crate::terminal::image::{GraphicsProtocol, detect_graphics_protocol};
            use crate::views::file_search::line_viewer::dim_area;
            use crate::views::shortcuts_bar::HintItem;
            let overlay_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: layout.shortcuts.y.saturating_sub(area.y),
            };
            let popup_width = ((overlay_area.width as u32 * 90) / 100)
                .max(28)
                .min(overlay_area.width as u32) as u16;
            let popup_height = ((overlay_area.height as u32 * 90) / 100)
                .max(8)
                .min(overlay_area.height as u32) as u16;
            let popup_x = overlay_area.x + (overlay_area.width.saturating_sub(popup_width)) / 2;
            let popup_y = overlay_area.y + (overlay_area.height.saturating_sub(popup_height)) / 2;
            let popup_rect = Rect::new(popup_x, popup_y, popup_width, popup_height);
            let mut image_escape_emitted = false;
            if popup_rect.width >= 10 && popup_rect.height >= 5 {
                dim_area(buf, overlay_area, theme.bg_base, 0.5);
                ratatui::widgets::Clear.render(popup_rect, buf);
                buf.set_style(
                    popup_rect,
                    Style::default().fg(theme.text_primary).bg(theme.bg_base),
                );
                let block = ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .border_style(Style::default().fg(theme.gray_dim))
                    .style(Style::default().bg(theme.bg_base));
                block.render(popup_rect, buf);
                let inner_cols = popup_rect.width.saturating_sub(2);
                let inner_rows = popup_rect.height.saturating_sub(2);
                let title_style = Style::default()
                    .fg(theme.text_primary)
                    .bg(theme.bg_base)
                    .add_modifier(ratatui::style::Modifier::BOLD);
                let dim_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
                let border_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
                let title_spans: Vec<ratatui::text::Span> = if viewer.loading {
                    let name = viewer.title.as_deref().unwrap_or("Loading...");
                    vec![
                        ratatui::text::Span::styled("\u{2500} ", border_style),
                        ratatui::text::Span::styled(name.to_owned(), title_style),
                        ratatui::text::Span::styled(" \u{2500}", border_style),
                    ]
                } else {
                    let name = viewer.title.as_deref().unwrap_or(&viewer.mime_type);
                    let dims = format!(" ({}\u{00d7}{})", viewer.image_width, viewer.image_height);
                    vec![
                        ratatui::text::Span::styled("\u{2500} ", border_style),
                        ratatui::text::Span::styled(name.to_owned(), title_style),
                        ratatui::text::Span::styled(dims, dim_style),
                        ratatui::text::Span::styled(" \u{2500}", border_style),
                    ]
                };
                viewer.modal_state.popup_area = Some(popup_rect);
                crate::views::modal_window::render_close_button(
                    buf,
                    popup_rect,
                    &mut viewer.modal_state,
                    &theme,
                );
                let close_width = viewer
                    .modal_state
                    .close_button_rect
                    .map_or(0, |r| r.width + 1);
                let tx = popup_rect.x + 2;
                let max_title_width = popup_rect.width.saturating_sub(2 + close_width + 1);
                {
                    use unicode_width::UnicodeWidthStr;
                    let mut cursor: u16 = 0;
                    for span in &title_spans {
                        let w = span.content.width() as u16;
                        if cursor + w > max_title_width {
                            break;
                        }
                        buf.set_span_safe(tx + cursor, popup_rect.y, span, w);
                        cursor += w;
                    }
                }
                if viewer.loading {
                    if inner_cols > 0 && inner_rows > 0 {
                        use crate::views::turn_status::SPINNER_DIVISOR;
                        use unicode_width::UnicodeWidthStr;
                        let tick = self.scrollback.animation_tick();
                        let frames = crate::glyphs::braille_spinner_frames();
                        let frame = match frames.len() {
                            0 => "",
                            n => frames
                                .get((tick / SPINNER_DIVISOR) as usize % n)
                                .copied()
                                .unwrap_or(""),
                        };
                        let loading = format!("{} Loading...", frame);
                        let lw = loading.width() as u16;
                        let lx = popup_rect.x + 1 + inner_cols.saturating_sub(lw) / 2;
                        let ly = popup_rect.y + 1 + inner_rows / 2;
                        buf.set_span_safe(
                            lx,
                            ly,
                            &ratatui::text::Span::styled(
                                loading,
                                Style::default().fg(theme.gray_dim).bg(theme.bg_base),
                            ),
                            lw,
                        );
                    }
                } else {
                    let rendered_pixel = crate::terminal::overlay::static_centered(
                        &viewer.display_bytes,
                        viewer.image_width,
                        viewer.image_height,
                        popup_rect,
                        viewer.overlay_owner_id,
                    )
                    .is_some_and(|esc| {
                        prompt_post_flush = Some(esc.into());
                        image_escape_emitted = true;
                        true
                    });
                    if inner_cols > 0 && inner_rows > 0 {
                        let inner_rect = ratatui::layout::Rect::new(
                            popup_rect.x + 1,
                            popup_rect.y + 1,
                            inner_cols,
                            inner_rows,
                        );
                        if !rendered_pixel {
                            let meta_lines = vec![
                                ratatui::text::Line::from(""),
                                ratatui::text::Line::from(format!(
                                    "  {}x{} {}",
                                    viewer.image_width, viewer.image_height, viewer.mime_type,
                                )),
                                ratatui::text::Line::from(""),
                                ratatui::text::Line::from("  Press Esc to close"),
                            ];
                            ratatui::widgets::Paragraph::new(meta_lines)
                                .style(Style::default().fg(theme.gray_dim).bg(theme.bg_base))
                                .render(inner_rect, buf);
                        } else {
                            let loading = "Loading...";
                            let lw = loading.len() as u16;
                            let lx = inner_rect.x + inner_cols.saturating_sub(lw) / 2;
                            let ly = inner_rect.y + inner_rows / 2;
                            buf.set_span_safe(
                                lx,
                                ly,
                                &ratatui::text::Span::styled(
                                    loading,
                                    Style::default().fg(theme.gray_dim).bg(theme.bg_base),
                                ),
                                lw,
                            );
                        }
                    }
                }
            }
            if !image_escape_emitted && detect_graphics_protocol() == GraphicsProtocol::Kitty {
                let clear = crate::terminal::overlay::clear_kitty();
                prompt_post_flush = Some(clear.into());
            }
            let hints = vec![HintItem::new(key!(Esc), "close")];
            ShortcutsBar::new(&hints).render(layout.shortcuts, buf);
            self.pane_areas = layout.pane_areas();
            return (None, prompt_post_flush);
        }
        if let Some(ref viewer) = self.video_viewer {
            use crate::terminal::image::{GraphicsProtocol, detect_graphics_protocol};
            use crate::views::shortcuts_bar::HintItem;
            let overlay_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: layout.shortcuts.y.saturating_sub(area.y),
            };
            let mut video_escape_emitted = false;
            if let Some(popup_rect) = crate::render::video_overlay::render_video_overlay(
                buf,
                overlay_area,
                viewer,
                theme.bg_base,
                theme.text_primary,
                theme.gray_dim,
            ) {
                if let Some(esc) = crate::terminal::overlay::volatile_centered(
                    viewer.current_frame_data(),
                    viewer.video_width,
                    viewer.video_height,
                    popup_rect,
                ) {
                    prompt_post_flush = Some(esc.into());
                    video_escape_emitted = true;
                }
                if !video_escape_emitted && detect_graphics_protocol() == GraphicsProtocol::Kitty {
                    let clear = crate::terminal::overlay::clear_kitty();
                    prompt_post_flush = Some(clear.into());
                }
            }
            let play_label = if viewer.playing { "pause" } else { "play" };
            let hints = vec![
                HintItem::new(key!(Esc), "close"),
                HintItem::new(key!(' '), play_label),
                HintItem::new(key!(Left), "back"),
                HintItem::new(key!(Right), "fwd"),
            ];
            ShortcutsBar::new(&hints).render(layout.shortcuts, buf);
            self.pane_areas = layout.pane_areas();
            return (None, prompt_post_flush);
        }
        if let Some(gboom) = self.gboom.as_mut() {
            use crate::terminal::image::{GraphicsProtocol, detect_graphics_protocol};
            use crate::views::shortcuts_bar::HintItem;
            let overlay_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: layout.shortcuts.y.saturating_sub(area.y),
            };
            let mut gboom_escape_emitted = false;
            let hud = gboom.hud();
            if let Some(popup_rect) = crate::render::gboom_overlay::render_gboom_overlay(
                buf,
                overlay_area,
                &hud,
                theme.bg_base,
                theme.text_primary,
                theme.gray_dim,
            ) {
                gboom.set_mouse_region(
                    popup_rect.x,
                    popup_rect.y,
                    popup_rect.width,
                    popup_rect.height,
                );
                let inner_cols = popup_rect.width.saturating_sub(2);
                let inner_rows = popup_rect.height.saturating_sub(2);
                if inner_cols >= 10 && inner_rows >= 4 {
                    let (px_w, px_h) =
                        crate::gboom::GboomState::frame_size_for_cells(inner_cols, inner_rows);
                    if let Some(png) = gboom.frame_png(px_w, px_h)
                        && let Some(esc) = crate::terminal::overlay::volatile_image(
                            png,
                            inner_cols,
                            inner_rows,
                            popup_rect.x + 1,
                            popup_rect.y + 1,
                        )
                    {
                        prompt_post_flush = Some(esc.into());
                        gboom_escape_emitted = true;
                    }
                }
            } else {
                gboom.clear_mouse_region();
            }
            if !gboom_escape_emitted && detect_graphics_protocol() == GraphicsProtocol::Kitty {
                let clear = crate::terminal::overlay::clear_kitty();
                prompt_post_flush = Some(clear.into());
            }
            let hints = vec![
                HintItem::new(key!(Esc), "quit"),
                HintItem::new(key!(' '), "fire"),
            ];
            ShortcutsBar::new(&hints).render(layout.shortcuts, buf);
            self.pane_areas = layout.pane_areas();
            return (None, prompt_post_flush);
        }
        let block_viewer_toast = self.active_toast_message().map(|s| s.to_string());
        if let Some(ref mut viewer) = self.block_viewer {
            use ratatui::style::Modifier;
            use ratatui::widgets::{Block as RBlock, BorderType, Borders, Clear, Widget};
            let overlay_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: layout.shortcuts.y.saturating_sub(area.y),
            };
            let entry = if viewer.kind == crate::views::block_viewer::ViewerKind::PlainText {
                Some(crate::scrollback::entry::ScrollbackEntry::new(
                    crate::scrollback::block::RenderBlock::system(String::new()),
                ))
            } else {
                self.scrollback
                    .get_by_id(viewer.entry_id)
                    .cloned()
                    .or_else(|| {
                        if viewer.kind != crate::views::block_viewer::ViewerKind::BgTask {
                            return None;
                        }
                        let task = self.session.bg_tasks.get(viewer.bg_task_id.as_deref()?)?;
                        use crate::app::agent::BgTaskStatus;
                        use crate::scrollback::blocks::BgTaskBlock;
                        let block = match task.status {
                            BgTaskStatus::Running => {
                                BgTaskBlock::started(&task.command, &task.task_id)
                            }
                            BgTaskStatus::Done => {
                                BgTaskBlock::completed(&task.command, &task.task_id, task.elapsed())
                            }
                            BgTaskStatus::Failed => BgTaskBlock::failed(
                                &task.command,
                                &task.task_id,
                                task.elapsed(),
                                task.exit_code,
                                task.signal.clone(),
                            ),
                        }
                        .with_description(task.description.clone());
                        Some(crate::scrollback::entry::ScrollbackEntry::new(
                            crate::scrollback::block::RenderBlock::BgTask(block),
                        ))
                    })
            };
            let Some(entry) = entry else {
                self.clear_block_viewer();
                self.pane_areas = layout.pane_areas();
                return (prompt_cursor_pos, prompt_post_flush);
            };
            let popup_w = ((overlay_area.width as f32 * 0.95) as u16)
                .max(60)
                .min(overlay_area.width);
            let popup_h = ((overlay_area.height as f32 * 0.92) as u16)
                .max(12)
                .min(overlay_area.height.saturating_sub(2));
            let h_pad = 2u16;
            let inner_w = popup_w.saturating_sub(2);
            let content_w = inner_w.saturating_sub(h_pad * 2);
            if popup_h.saturating_sub(2) < 3 || inner_w < 10 {
                self.pane_areas = layout.pane_areas();
                return (prompt_cursor_pos, prompt_post_flush);
            }
            let popup_x = overlay_area.x + (overlay_area.width.saturating_sub(popup_w)) / 2;
            let popup_y = overlay_area.y + (overlay_area.height.saturating_sub(popup_h)) / 2;
            let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);
            let base_style = Style::default().fg(theme.text_primary).bg(theme.bg_base);
            let border = RBlock::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.gray_dim))
                .style(base_style);
            let inner = border.inner(popup_area);
            if inner.height < 3 || inner.width < 10 {
                self.pane_areas = layout.pane_areas();
                return (prompt_cursor_pos, prompt_post_flush);
            }
            crate::views::file_search::line_viewer::dim_area(buf, overlay_area, theme.bg_base, 0.5);
            Clear.render(popup_area, buf);
            buf.set_style(popup_area, base_style);
            border.render(popup_area, buf);
            let close_chars: &[char] = &['[', 'x', ']'];
            let close_w_px = close_chars.len() as u16;
            let close_x = inner.x + inner.width.saturating_sub(close_w_px + 1);
            let close_y = inner.y;
            let close_rect = Rect::new(close_x, close_y, close_w_px, 1);
            for (i, &ch) in close_chars.iter().enumerate() {
                if let Some(cell) = buf.cell_mut((close_x + i as u16, close_y)) {
                    cell.set_char(ch);
                    let style = if viewer.modal.close_hovered {
                        Style::default()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.gray_dim)
                    };
                    cell.set_style(style);
                }
            }
            viewer.modal.close_button_rect = Some(close_rect);
            viewer.modal.popup_area = Some(popup_area);
            let preamble_ctx = crate::scrollback::types::BlockContext {
                mode: crate::scrollback::types::DisplayMode::Expanded,
                is_running: entry.is_running,
                width: content_w,
                raw: entry.raw,
                max_lines: None,
                appearance: appearance.clone(),
                is_selected: false,
                cwd: Some(self.session.cwd.clone()),
            };
            let preamble = entry.block.preamble(&preamble_ctx);
            let mut prepend_lines: Vec<ratatui::text::Line<'static>> = preamble
                .as_ref()
                .map(|text| {
                    text.lines
                        .iter()
                        .flat_map(|line| {
                            crate::render::wrapping::wrap_header_flush(
                                line.clone(),
                                content_w as usize,
                                0,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !prepend_lines.is_empty() {
                prepend_lines.push(ratatui::text::Line::from(""));
            }
            let content_x = inner.x + h_pad;
            let content_top = inner.y + 1;
            let content_height = inner.height.saturating_sub(1);
            let content_area = Rect {
                x: content_x,
                y: content_top,
                width: content_w,
                height: content_height,
            };
            viewer.render_content(content_area, buf, &entry, true, &prepend_lines);
            viewer.render_text_drag_overlay(buf);
            let has_input_bar =
                viewer.list_state.input_mode().is_some() || viewer.list_state.matcher().is_some();
            let in_visual = viewer.list_state.visual_mode;
            if (has_input_bar || in_visual) && content_area.height > 2 {
                let div_y = content_area.y + content_area.height - 2;
                for x in inner.x..inner.x + inner.width {
                    if let Some(cell) = buf.cell_mut((x, div_y)) {
                        cell.reset();
                        cell.set_char('\u{2500}');
                        cell.fg = theme.gray_dim;
                        cell.bg = theme.bg_base;
                    }
                }
                if in_visual && !has_input_bar {
                    let status_y = div_y + 1;
                    for x in inner.x..inner.x + inner.width {
                        if let Some(cell) = buf.cell_mut((x, status_y)) {
                            cell.reset();
                            cell.set_char(' ');
                            cell.fg = theme.text_secondary;
                            cell.bg = theme.bg_base;
                        }
                    }
                    let n = viewer.list_state.copy_range().map(|r| r.len()).unwrap_or(1);
                    let s = if n == 1 { "" } else { "s" };
                    let status = format!("Selected: {n} line{s}");
                    let status_style = Style::default().fg(theme.text_secondary).bg(theme.bg_base);
                    buf.set_string(content_x, status_y, &status, status_style);
                }
            }
            if let Some(ref msg) = block_viewer_toast
                && popup_area.height > 2
                && let Some(toast_text) = fit_toast_text(msg, popup_area.width.saturating_sub(1))
            {
                let w = toast_text.chars().count() as u16;
                let tx = popup_area.right().saturating_sub(w + 2);
                let ty = popup_area.bottom().saturating_sub(2);
                for (i, ch) in toast_text.chars().enumerate() {
                    if let Some(cell) = buf.cell_mut((tx + i as u16, ty)) {
                        cell.set_char(ch);
                        cell.fg = theme.accent_user;
                        cell.bg = theme.bg_base;
                        cell.modifier = ratatui::prelude::Modifier::BOLD;
                    }
                }
            }
            let hints = viewer.shortcuts_hints();
            ShortcutsBar::new(&hints).render(layout.shortcuts, buf);
            self.pane_areas = layout.pane_areas();
            return (prompt_cursor_pos, prompt_post_flush);
        }
        if let Some(ref mut modal_state) = self.agents_modal {
            let overlay_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: layout.shortcuts.y.saturating_sub(area.y).saturating_sub(1),
            };
            let compact = self.scrollback.appearance().prompt.compact;
            let theme = Theme::current();
            crate::views::agents_modal::render_agents_modal(
                buf,
                overlay_area,
                modal_state,
                compact,
                &theme,
            );
            if let Some(ref mut detail) = self.persona_detail {
                crate::views::persona_detail::render_persona_detail(
                    buf,
                    overlay_area,
                    detail,
                    &theme,
                    compact,
                );
            }
            self.pane_areas = layout.pane_areas();
            return (None, crate::terminal::overlay::clear().map(Into::into));
        }
        if let Some(ref mut modal_state) = self.feedback_modal {
            let overlay_area = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: layout.shortcuts.y.saturating_sub(area.y).saturating_sub(1),
            };
            let compact = self.scrollback.appearance().prompt.compact;
            let rendered = modal_state.render(buf, overlay_area, &theme, compact);
            let (cursor, post_flush) = match rendered {
                Some(frame) => (frame.cursor, frame.post_flush),
                None => (None, None),
            };
            let post_flush =
                post_flush.or_else(|| crate::terminal::overlay::clear().map(Into::into));
            self.pane_areas = layout.pane_areas();
            return (cursor, post_flush);
        }
        if let Some(ref mut modal_state) = self.extensions_modal {
            use crate::views::extensions_modal::render_extensions_modal;
            use crate::views::shortcuts_bar::HintItem;
            let is_fullscreen = matches!(
                modal_state.picker_state.mode,
                crate::views::picker::PickerMode::FullScreen
            );
            let overlay_area = if is_fullscreen {
                area
            } else {
                Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: layout.shortcuts.y.saturating_sub(area.y).saturating_sub(1),
                }
            };
            let compact = self.scrollback.appearance().prompt.compact;
            let tick = self.scrollback.animation_tick();
            render_extensions_modal(
                buf,
                overlay_area,
                modal_state,
                Some(layout.shortcuts),
                compact,
                tick,
            );
            if crate::hyperlink_route::hyperlink_route().emit_osc8 {
                Self::push_managed_connectors_wait_link_spans(link_spans_out, modal_state);
            }
            if modal_state.input.is_some() {
                let hints = vec![
                    HintItem::new(key!(Enter), "submit"),
                    HintItem::new(key!(Esc), "cancel"),
                ];
                ShortcutsBar::new(&hints).render(layout.shortcuts, buf);
            } else if modal_state.pending_action.is_some() {
                let hints = vec![HintItem::new(key!(Esc), "dismiss")];
                ShortcutsBar::new(&hints).render(layout.shortcuts, buf);
            } else if modal_state.picker_state.search_active
                && !modal_state.is_managed_connectors_wait()
            {
                let hints = vec![
                    HintItem::new(key!(Esc), "clear search"),
                    HintItem::new(key!(Enter), "keep filter"),
                ];
                ShortcutsBar::new(&hints).render(layout.shortcuts, buf);
            }
            self.pane_areas = layout.pane_areas();
            return (None, crate::terminal::overlay::clear().map(Into::into));
        }
        let dropdown_active = self.slash_dropdown_items_area.is_some()
            || self.dropdown_items_area.is_some()
            || self.completion_dropdown_items_area.is_some()
            || self.history_dropdown_area.is_some();
        let placements: &[_] = if dropdown_active {
            &[]
        } else {
            &scrollback_inline_media
        };
        self.inline_media_hits = InlineMediaHitAreas::default();
        if !placements.is_empty() {
            let mut all_escapes = String::new();
            let mut this_frame_ids: HashSet<u32> = HashSet::new();
            for placement in placements {
                let path = &placement.info.path;
                if let Some(fp_rect) = placement.filepath_screen_rect {
                    self.inline_media_hits
                        .filepath_areas
                        .push((fp_rect, path.clone()));
                }
                if let Some(open_rect) = placement.open_button_screen_rect {
                    self.inline_media_hits
                        .open_buttons
                        .push((open_rect, path.clone()));
                    continue;
                }
                if !crate::terminal::image::scrollback_inline_overlay_active() {
                    continue;
                }
                if !self.inline_media_cache.contains_key(path) && placement.screen_rect.height >= 1
                {
                    let rect = placement.screen_rect;
                    let center_x = |len: usize| rect.x + rect.width.saturating_sub(len as u16) / 2;
                    if placement.info.is_video && !crate::inline_media_ffmpeg::ffmpeg_available() {
                        use crate::inline_media_ffmpeg::{FFMPEG_HINT_TEXT, ffmpeg_install_cmd};
                        let warn = Style::default().fg(theme.warning);
                        buf.set_string_safe(
                            center_x(FFMPEG_HINT_TEXT.len()),
                            rect.y,
                            FFMPEG_HINT_TEXT,
                            warn,
                        );
                        if let Some(cmd) = ffmpeg_install_cmd() {
                            let dim = Style::default().fg(theme.gray_dim);
                            buf.set_string_safe(center_x(cmd.len()), rect.y + 1, cmd, dim);
                        }
                    } else {
                        let spinner_frames = crate::glyphs::braille_spinner_frames();
                        let tick = self.scrollback.current_tick() as usize;
                        let spinner = match spinner_frames.len() {
                            0 => "",
                            n => spinner_frames.get(tick % n).copied().unwrap_or(""),
                        };
                        let label = format!("{spinner} Loading...");
                        let cy = rect.y + rect.height / 2;
                        buf.set_string_safe(
                            center_x(label.len()),
                            cy,
                            &label,
                            Style::default().fg(theme.gray_dim),
                        );
                    }
                }
                if let Some(esc) = self.build_inline_media_escapes(placement) {
                    all_escapes.push_str(&esc);
                    if let Some(&id) = self.inline_media_ids.get(path) {
                        this_frame_ids.insert(id);
                    }
                    let rect = placement.screen_rect;
                    if !placement.has_button_row {
                        continue;
                    }
                    let button_y = rect.y + rect.height + 1;
                    let sb_area = self.pane_areas.scrollback;
                    let button_visible =
                        button_y >= sb_area.y && button_y < sb_area.y + sb_area.height;
                    if placement.info.is_video {
                        self.inline_media_hits
                            .video_play_areas
                            .push((rect, path.clone()));
                        if button_visible {
                            let is_playing = matches!(
                                self.inline_video,
                                Some(ref vid) if vid.path == *path && !vid.finished
                            );
                            let play_label: String = if is_playing {
                                let vid = self.inline_video.as_ref().unwrap();
                                let dur_s = vid.frames.len() as f64 / vid.fps;
                                let pos_s = vid.current_frame as f64 / vid.fps;
                                format!(
                                    "{}:{:02} / {}:{:02}",
                                    pos_s as u32 / 60,
                                    pos_s as u32 % 60,
                                    dur_s as u32 / 60,
                                    dur_s as u32 % 60,
                                )
                            } else {
                                "[Play]".to_string()
                            };
                            let open_label = "[Open]";
                            let gap = 3u16;
                            let total = play_label.len() as u16 + gap + open_label.len() as u16;
                            let start_x = rect.x + rect.width.saturating_sub(total) / 2;
                            buf.set_string_safe(
                                start_x,
                                button_y,
                                &play_label,
                                Style::default().fg(theme.gray),
                            );
                            if !is_playing {
                                self.inline_media_hits.play_buttons.push((
                                    Rect {
                                        x: start_x,
                                        y: button_y,
                                        width: play_label.len() as u16,
                                        height: 1,
                                    },
                                    path.clone(),
                                ));
                            }
                            let open_x = start_x + play_label.len() as u16 + gap;
                            buf.set_string_safe(
                                open_x,
                                button_y,
                                open_label,
                                Style::default().fg(theme.gray),
                            );
                            self.inline_media_hits.open_buttons.push((
                                Rect {
                                    x: open_x,
                                    y: button_y,
                                    width: open_label.len() as u16,
                                    height: 1,
                                },
                                path.clone(),
                            ));
                        }
                    } else {
                        self.inline_media_hits
                            .media_areas
                            .push((rect, path.clone()));
                        if button_visible {
                            let open_label = "[Open]";
                            let copy_label = "[Copy]";
                            let gap = 3u16;
                            let total = open_label.len() as u16 + gap + copy_label.len() as u16;
                            let start_x = rect.x + rect.width.saturating_sub(total) / 2;
                            buf.set_string_safe(
                                start_x,
                                button_y,
                                open_label,
                                Style::default().fg(theme.gray),
                            );
                            self.inline_media_hits.open_buttons.push((
                                Rect {
                                    x: start_x,
                                    y: button_y,
                                    width: open_label.len() as u16,
                                    height: 1,
                                },
                                path.clone(),
                            ));
                            let copy_x = start_x + open_label.len() as u16 + gap;
                            buf.set_string_safe(
                                copy_x,
                                button_y,
                                copy_label,
                                Style::default().fg(theme.gray),
                            );
                            self.inline_media_hits.copy_image_buttons.push((
                                Rect {
                                    x: copy_x,
                                    y: button_y,
                                    width: copy_label.len() as u16,
                                    height: 1,
                                },
                                path.clone(),
                            ));
                        }
                    }
                }
            }
            for &old_id in &self.last_placed_ids {
                if !this_frame_ids.contains(&old_id) {
                    all_escapes.push_str(&crate::terminal::image::clear_kitty_image(old_id));
                    self.inline_media_ids.retain(|_, &mut v| v != old_id);
                    self.inline_media_iterm_emitted
                        .retain(|p, _| self.inline_media_ids.contains_key(p));
                }
            }
            self.last_placed_ids = this_frame_ids;
            if !all_escapes.is_empty() {
                self.inline_media_active = true;
                match prompt_post_flush.as_mut() {
                    Some(existing) => existing.append_plain(&all_escapes),
                    None => {
                        prompt_post_flush =
                            Some(crate::terminal::overlay::PostFlush::plain(all_escapes));
                    }
                }
            } else if self.inline_media_active {
                self.inline_media_active = false;
                let mut clear_esc = String::new();
                for &id in self.inline_media_ids.values() {
                    clear_esc.push_str(&crate::terminal::image::clear_kitty_image(id));
                }
                self.inline_media_ids.clear();
                self.inline_media_iterm_emitted.clear();
                self.last_placed_ids.clear();
                if !clear_esc.is_empty() {
                    match prompt_post_flush.as_mut() {
                        Some(existing) => existing.append_plain(&clear_esc),
                        None => {
                            prompt_post_flush =
                                Some(crate::terminal::overlay::PostFlush::plain(clear_esc));
                        }
                    }
                }
            }
        } else if self.inline_media_active {
            self.inline_media_active = false;
            self.stop_inline_playback();
            let mut clear_esc = String::new();
            for &id in self.inline_media_ids.values() {
                clear_esc.push_str(&crate::terminal::image::clear_kitty_image(id));
            }
            self.inline_media_iterm_emitted.clear();
            self.inline_media_ids.clear();
            self.last_placed_ids.clear();
            if !clear_esc.is_empty() {
                match prompt_post_flush.as_mut() {
                    Some(existing) => existing.append_plain(&clear_esc),
                    None => {
                        prompt_post_flush =
                            Some(crate::terminal::overlay::PostFlush::plain(clear_esc));
                    }
                }
            }
        }
        if !dropdown_active {
            self.paint_diagram_affordances(buf, scrollback_diagram_affordances, &theme);
        }
        if !self.inline_media_active
            && prompt_post_flush.is_none()
            && let Some(clear) = crate::terminal::overlay::clear()
        {
            prompt_post_flush = Some(clear.into());
        }
        if self.show_goal_detail
            && let Some(ref goal) = self.goal_state
        {
            let todos = self.todo.todos();
            let overlay_rect = crate::views::goal_detail::goal_detail_area(area, goal, todos);
            let tick = self.tasks.tick_count() as usize;
            let active_subagent_tokens = self.live_standalone_subagent_tokens();
            let close_rect = crate::views::goal_detail::render_goal_detail(
                buf,
                overlay_rect,
                goal,
                todos,
                tick,
                self.context_state.as_ref().map(|c| c.used),
                active_subagent_tokens,
                self.hit_goal_close.hovered,
            );
            self.hit_goal_close.rect = close_rect;
            self.frame_occluder_rects.push(overlay_rect);
        }
        if self.show_workflows {
            let runs = self.workflow_runs_newest_first();
            let mut view = self.workflows_view.clone();
            view.normalize(&runs);
            let tick = self.tasks.tick_count() as usize;
            let live: crate::views::workflows::WorkflowAgentLiveMap = self
                .subagent_sessions
                .iter()
                .filter(|(_, info)| info.attempt.workflow_run_id.is_some())
                .map(|(id, info)| {
                    (
                        id.clone(),
                        crate::views::workflows::WorkflowAgentLiveStatus {
                            activity: info.attempt.activity_label.clone(),
                            context_tokens: info.attempt.tokens_used,
                            context_window_tokens: info.attempt.context_window_tokens,
                            elapsed_ms: Some(info.display_elapsed().as_millis() as u64),
                        },
                    )
                })
                .collect();
            let popup =
                crate::views::workflows::render_workflows(buf, area, &runs, &mut view, tick, &live);
            self.workflows_view = view;
            if let Some(popup) = popup {
                self.frame_occluder_rects.push(popup);
            }
        }
        self.pane_areas = layout.pane_areas();
        {
            let route = crate::hyperlink_route::hyperlink_route();
            if route.emit_osc8 {
                let occluders = &self.frame_occluder_rects;
                let emit_id = route.emit_id;
                *link_spans_out = self
                    .last_link_overlay
                    .links()
                    .iter()
                    .filter(|link| {
                        !occluders.iter().any(|r| {
                            link.screen_row >= r.y
                                && link.screen_row < r.y.saturating_add(r.height)
                                && link.col_start < r.x.saturating_add(r.width)
                                && r.x < link.col_end
                        })
                    })
                    .filter_map(|link| {
                        crate::render::osc8::resolve_link_target_with_presentation(
                            &link.target,
                            link.presentation,
                        )
                        .and_then(|resolved| resolved.osc8_url)
                        .map(|url| distill_ratatui_inline::LinkSpan {
                            row: link.screen_row,
                            col_start: link.col_start,
                            col_end: link.col_end,
                            url,
                            id: if emit_id { link.id } else { None },
                        })
                    })
                    .collect();
                self.push_promo_cta_link_span(
                    link_spans_out,
                    banner_announcements,
                    hidden_announcement_ids,
                );
                self.push_upgrade_cta_link_span(
                    link_spans_out,
                    banner_announcements,
                    hidden_announcement_ids,
                );
                self.push_status_line_link_spans(
                    link_spans_out,
                    std::mem::take(&mut status_line_link_spans),
                );
            }
        }
        let on_link = self.hovered_link_idx.is_some();
        if supports_osc22() && on_link != self.last_pointer_on_link {
            self.last_pointer_on_link = on_link;
            use crossterm::Command;
            let mut seq = String::new();
            if on_link {
                let _ = crate::terminal::SetPointerCursor.write_ansi(&mut seq);
            } else {
                let _ = crate::terminal::SetDefaultCursor.write_ansi(&mut seq);
            }
            match prompt_post_flush {
                Some(ref mut existing) => existing.append_plain(&seq),
                None => {
                    prompt_post_flush = Some(crate::terminal::overlay::PostFlush::plain(seq));
                }
            }
        }
        let cursor = if self.inline_edit.is_some() {
            inline_edit_cursor
        } else {
            prompt_cursor_pos
        };
        (cursor, prompt_post_flush)
    }
}
/// Draw one ▼/▲ scroll-indicator arrow centered on row `y`, or clear its hit area when hidden (`y: None`).
/// The unconditional set-or-clear is the point: a hit rect must never outlive the frame that painted its arrow.
/// An invisible click target would keep firing under whatever covers it (e.g. an open block viewer).
fn draw_scroll_arrow(
    buf: &mut Buffer,
    theme: &Theme,
    center_x: u16,
    y: Option<u16>,
    symbol: &str,
    hit: &mut super::HitArea,
) {
    let Some(y) = y else {
        hit.clear();
        return;
    };
    let style = Style::default().fg(if hit.hovered {
        theme.gray_bright
    } else {
        theme.gray
    });
    if let Some(cell) = buf.cell_mut((center_x, y)) {
        cell.set_symbol(symbol);
        cell.set_style(style);
    }
    hit.set(Some(Rect::new(center_x.saturating_sub(1), y, 3, 1)));
}
/// Pad `msg` for the toast slot, truncating with a trailing ellipsis when it cannot fit in `avail_width` columns.
/// (Long clipboard toasts embed backup file paths; dropping the whole toast would hide the copy feedback entirely.)
/// Returns `None` only when the slot is too narrow for any text.
fn fit_toast_text(msg: &str, avail_width: u16) -> Option<String> {
    let max_msg_chars = (avail_width as usize).saturating_sub(4);
    if max_msg_chars == 0 {
        return None;
    }
    let msg_chars = msg.chars().count();
    if msg_chars <= max_msg_chars {
        return Some(format!(" {msg} "));
    }
    let truncated: String = msg.chars().take(max_msg_chars.saturating_sub(1)).collect();
    Some(format!(" {}… ", truncated.trim_end()))
}
#[cfg(test)]
mod toast_fit_tests {
    use super::fit_toast_text;
    #[test]
    fn short_message_is_padded_untouched() {
        assert_eq!(fit_toast_text("Copied!", 40).as_deref(), Some(" Copied! "));
    }
    #[test]
    fn long_message_truncates_with_ellipsis_instead_of_vanishing() {
        let msg = "Copied via OSC 52 — also saved to /tmp/grok-0/last-copy.txt. If paste fails, hold Shift (or Fn) and drag to select & copy natively.";
        let fitted = fit_toast_text(msg, 60).expect("must render truncated");
        assert!(fitted.chars().count() <= 58);
        assert!(fitted.ends_with("… "));
        assert!(fitted.contains("also saved to"));
    }
    #[test]
    fn zero_width_slot_yields_none() {
        assert_eq!(fit_toast_text("Copied!", 4), None);
        assert_eq!(fit_toast_text("Copied!", 0), None);
    }
}
#[cfg(test)]
mod goal_subagent_token_tests {
    use super::super::test_fixtures::make_agent;
    use crate::app::subagent::{SubagentInfo, test_support::make_info};
    fn child(child_session_id: &str, tokens: u64, workflow_run_id: Option<&str>) -> SubagentInfo {
        let mut info = make_info();
        info.subagent_id = child_session_id.into();
        info.child_session_id = child_session_id.into();
        info.attempt.tokens_used = Some(tokens);
        info.attempt.workflow_run_id = workflow_run_id.map(Into::into);
        info
    }
    #[test]
    fn live_standalone_subagent_tokens_exclude_workflows_and_finished_attempts() {
        let mut agent = make_agent();
        agent
            .subagent_sessions
            .insert("standalone".to_string(), child("standalone", 60, None));
        agent.subagent_sessions.insert(
            "workflow".to_string(),
            child("workflow", 90, Some("workflow-1")),
        );
        let mut finished = child("finished", 120, None);
        finished.set_finished_for_test(true);
        agent
            .subagent_sessions
            .insert("finished".to_string(), finished);
        assert_eq!(agent.live_standalone_subagent_tokens(), 60);
    }
    #[test]
    fn live_standalone_subagent_tokens_include_completed_wake_attempts() {
        let mut agent = make_agent();
        let mut standalone = child("standalone", 30, None);
        standalone.completed_attempt_tokens = 100;
        agent
            .subagent_sessions
            .insert("standalone".to_string(), standalone);
        assert_eq!(agent.live_standalone_subagent_tokens(), 130);
    }
}
#[cfg(test)]
mod selection_state_tests {
    use super::super::test_fixtures::make_agent;
    use super::*;
    use crate::scrollback::text_selection::ResolvedSelectableLine;
    use crate::scrollback::types::SelectionBoundary;
    use std::sync::Arc;
    #[test]
    fn frame_reset_clears_model_and_companion_together() {
        let mut model = ResolvedSelectionModel::default();
        let line = ResolvedSelectableLine {
            entry_idx: 0,
            range_id: 0,
            block_line_idx: 0,
            screen_y: 0,
            screen_x: 0,
            selectable_cols: 0..3,
            text: "foo".to_string(),
            painted_region: None,
            joiner_to_previous: None,
        };
        let mut boundaries = ResolvedSelectionBoundaries::default();
        boundaries.push(
            &line,
            Arc::new(SelectionBoundary::new("   ".to_string(), String::new())),
        );
        model.push_line(line);
        let mut agent = make_agent();
        agent.update_scrollback_selection_state(model, boundaries);
        agent.clear_scrollback_selection_state();
        assert!(agent.last_scrollback_selection_model.ranges.is_empty());
        assert!(agent.last_scrollback_selection_boundaries.is_empty());
    }
}
#[cfg(test)]
mod voice_recording_overlay_tests {
    use super::super::test_fixtures::make_agent;
    use super::super::test_fixtures::make_plan_approval_view_state;
    use super::AgentView;
    use crate::actions::ActionRegistry;
    use crate::app::bundle::BundleState;
    use crate::scrollback::render::ScratchBuffer;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    /// Agent with the plan-approval view (and its line-viewer overlay) open.
    fn plan_approval_agent() -> AgentView {
        let mut agent = make_agent();
        agent.plan_approval_view = Some(make_plan_approval_view_state());
        agent.reopen_plan_approval();
        assert!(agent.line_viewer.is_some(), "approval must open the viewer");
        agent
    }
    /// Render `agent` with the given voice state and return the buffer text.
    fn render_text(agent: &mut AgentView, listening: bool) -> String {
        let reg = ActionRegistry::defaults();
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut scratch = ScratchBuffer::new();
        agent.draw(
            area,
            &mut buf,
            &reg,
            &mut scratch,
            None,
            false,
            crate::app::agent_view::BannerSlotParams::none(),
            &BundleState::default(),
            false,
            &mut Vec::new(),
            super::AppRenderParams {
                voice_available: listening,
                voice_listening: listening,
                ..Default::default()
            },
        );
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }
    /// The plan approval's line-viewer overlay used to paint over the `voice_recording` row.
    /// That left a live mic (Ctrl+Space / F8 still work there) with no visible "Recording" indicator.
    /// The overlay must stop above the record indicator row.
    #[test]
    fn recording_row_visible_while_plan_approval_open() {
        let mut agent = plan_approval_agent();
        let text = render_text(&mut agent, true);
        assert!(
            text.contains("Recording"),
            "record indicator must stay visible under the plan approval viewer:\n{text}"
        );
    }
    /// While voice is idle no indicator row exists, so the overlay keeps reaching the prompt as before.
    #[test]
    fn no_recording_row_when_not_listening_in_plan_approval() {
        let mut agent = plan_approval_agent();
        let text = render_text(&mut agent, false);
        assert!(
            !text.contains("Recording"),
            "no record indicator when voice is idle:\n{text}"
        );
    }
}
#[cfg(test)]
mod overlay_cycle_hint_tests {
    use super::super::test_fixtures::make_agent;
    use crate::actions::ActionRegistry;
    use crate::app::bundle::BundleState;
    use crate::scrollback::render::ScratchBuffer;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    fn draw_overlay_footer(
        can_cycle: bool,
        workspace_dashboard_enabled: bool,
        background_work: bool,
        bound: bool,
    ) -> String {
        let mut agent = make_agent();
        if bound {
            agent.session.session_id = Some("overlay-session".into());
        }
        if background_work {
            agent.session.scheduled_tasks.insert(
                "loop-1".into(),
                crate::app::agent::ScheduledTaskInfo {
                    task_id: "loop-1".into(),
                    prompt: "keep going".into(),
                    human_schedule: "every 5m".into(),
                    created_at: std::time::Instant::now(),
                    next_fire_at: None,
                    tag: "loop".into(),
                    last_subagent_id: None,
                },
            );
        }
        let reg = ActionRegistry::defaults();
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        let mut scratch = ScratchBuffer::new();
        agent.draw(
            area,
            &mut buf,
            &reg,
            &mut scratch,
            None,
            false,
            crate::app::agent_view::BannerSlotParams::none(),
            &BundleState::default(),
            true,
            &mut Vec::new(),
            super::AppRenderParams {
                workspace_dashboard_enabled,
                overlay_header: super::OverlayHeader {
                    title: None,
                    position: Some((1, if can_cycle { 2 } else { 1 })),
                },
                ..Default::default()
            },
        );
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn prev_next_hint_shown_when_overlay_can_cycle() {
        let text = draw_overlay_footer(true, false, false, true);
        assert!(
            text.contains("prev/next agent"),
            "footer must advertise cycle keys when can_cycle:\n{text}"
        );
    }
    #[test]
    fn prev_next_hint_hidden_when_overlay_cannot_cycle() {
        let text = draw_overlay_footer(false, false, false, true);
        assert!(
            !text.contains("prev/next agent"),
            "footer must not advertise cycle keys for a single agent:\n{text}"
        );
        assert!(
            text.contains("dashboard"),
            "back-to-dashboard hint still shown:\n{text}"
        );
    }
    #[test]
    fn settled_workspace_overlay_advertises_archive() {
        let text = draw_overlay_footer(false, true, false, true);
        assert!(text.contains("Ctrl+x:archive"), "{text}");
        assert!(!text.contains("Ctrl+x:stop"), "{text}");
    }
    #[test]
    fn unbound_workspace_overlay_advertises_close() {
        let text = draw_overlay_footer(false, true, false, false);
        assert!(text.contains("Ctrl+x:close"), "{text}");
        assert!(!text.contains("Ctrl+x:archive"), "{text}");
    }
    #[test]
    fn workspace_cheatsheet_uses_the_same_ctrl_x_action_as_the_footer() {
        let registry = ActionRegistry::defaults();
        let mut agent = make_agent();
        agent.in_dashboard_overlay = true;
        agent.workspace_dashboard_enabled = true;
        let stop_key = registry
            .find(crate::actions::ActionId::DashboardOverlayStop)
            .unwrap()
            .default_key;
        let ctrl_x_label = |agent: &super::AgentView| {
            agent
                .current_shortcut_hints(&registry)
                .into_iter()
                .find(|hint| hint.keys.contains(&stop_key))
                .map(|hint| hint.label)
        };
        assert_eq!(ctrl_x_label(&agent).as_deref(), Some("close"));
        agent.session.session_id = Some("workspace-session".into());
        assert_eq!(ctrl_x_label(&agent).as_deref(), Some("archive"));
    }
    /// A subagent's fullscreen takeover keeps the parent's footer contract: an idle persisted parent reads `archive`, not the
    /// child's own `stop`, and the parent's pending confirmation shows through.
    #[test]
    fn nested_takeover_footer_uses_the_parent_stop_action_and_confirmation() {
        let reg = ActionRegistry::defaults();
        let area = Rect::new(0, 0, 100, 30);
        let render = |pending: Option<crate::views::shortcuts_bar::PendingHint>| {
            let mut parent = make_agent();
            parent.session.session_id = Some("overlay-session".into());
            parent.insert_test_child("child-sid".into(), Box::new(make_agent()));
            parent.active_subagent = Some("child-sid".into());
            let mut buf = Buffer::empty(area);
            let mut scratch = ScratchBuffer::new();
            parent.draw(
                area,
                &mut buf,
                &reg,
                &mut scratch,
                pending,
                false,
                crate::app::agent_view::BannerSlotParams::none(),
                &BundleState::default(),
                true,
                &mut Vec::new(),
                super::AppRenderParams {
                    workspace_dashboard_enabled: true,
                    ..Default::default()
                },
            );
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = render(None);
        assert!(text.contains("Ctrl+x:archive"), "{text}");
        assert!(!text.contains("Ctrl+x:stop"), "{text}");
        let confirming = render(Some(crate::views::shortcuts_bar::PendingHint {
            shortcut: crate::key!('x', CONTROL),
            label: "archive",
        }));
        assert!(
            confirming.contains("press again to archive"),
            "{confirming}"
        );
    }
    #[test]
    fn busy_workspace_overlay_and_v1_keep_stop_copy() {
        let busy_v2 = draw_overlay_footer(false, true, true, true);
        assert!(busy_v2.contains("Ctrl+x:stop"), "{busy_v2}");
        let settled_v1 = draw_overlay_footer(false, false, false, true);
        assert!(settled_v1.contains("Ctrl+x:stop"), "{settled_v1}");
    }
}
#[cfg(test)]
mod overlay_post_flush_tests {
    use super::super::test_fixtures::make_agent;
    use crate::actions::ActionRegistry;
    use crate::app::bundle::BundleState;
    use crate::scrollback::render::ScratchBuffer;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    fn draw(agent: &mut super::AgentView) -> Option<crate::terminal::overlay::PostFlush> {
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        let mut scratch = ScratchBuffer::new();
        agent
            .draw(
                area,
                &mut buf,
                &ActionRegistry::defaults(),
                &mut scratch,
                None,
                false,
                crate::app::agent_view::BannerSlotParams::none(),
                &BundleState::default(),
                false,
                &mut Vec::new(),
                super::AppRenderParams::default(),
            )
            .1
    }
    fn seed_static_owner(owner_id: u64) {
        let _ = crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, owner_id)
            .unwrap()
            .commit();
    }
    fn png() -> [u8; 8] {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
    }
    #[test]
    fn fullscreen_subagent_propagates_child_clear_to_emitter() {
        let _guard = crate::terminal::image::set_protocol_for_test(
            crate::terminal::image::GraphicsProtocol::Kitty,
        );
        crate::terminal::overlay::reset_owner();
        seed_static_owner(41);
        let mut parent = make_agent();
        parent.insert_test_child("child".into(), Box::new(make_agent()));
        parent.active_subagent = Some("child".into());
        let post_flush = draw(&mut parent).expect("child clear propagates");
        assert!(post_flush.as_str().contains("a=d"));
        let before_emit = crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, 41).unwrap();
        assert!(!before_emit.as_str().contains("a=T"));
        post_flush.write_to(&mut Vec::new()).unwrap();
        let after_emit = crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, 41).unwrap();
        assert!(after_emit.as_str().contains("a=T"));
    }
    #[test]
    fn active_modal_returns_clear_without_committing_discarded_state() {
        let _guard = crate::terminal::image::set_protocol_for_test(
            crate::terminal::image::GraphicsProtocol::Kitty,
        );
        crate::terminal::overlay::reset_owner();
        seed_static_owner(42);
        let mut agent = make_agent();
        agent.active_modal = Some(crate::views::modal::ActiveModal::CommandPalette {
            entries: Vec::new(),
            state: crate::views::picker::PickerState::default(),
            window: crate::views::modal_window::ModalWindowState::new(),
        });
        let post_flush = draw(&mut agent).expect("modal clear returned");
        assert!(post_flush.as_str().contains("a=d"));
        let before_emit = crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, 42).unwrap();
        assert!(!before_emit.as_str().contains("a=T"));
        post_flush.write_to(&mut Vec::new()).unwrap();
        let after_emit = crate::terminal::overlay::static_image(&png(), 20, 10, 0, 0, 42).unwrap();
        assert!(after_emit.as_str().contains("a=T"));
    }
}
#[cfg(test)]
mod permission_hint_tests {
    use super::super::test_fixtures::{make_agent, make_followup_permission_state};
    use crate::views::permission_view::PermissionFocus;
    use agent_client_protocol as acp;
    use ratatui::layout::Rect;
    use std::sync::Arc;
    const ALL_FOCUSES: [PermissionFocus; 3] = [
        PermissionFocus::Options,
        PermissionFocus::FollowupInput,
        PermissionFocus::PatternEdit,
    ];
    /// The footer Ctrl-F hint must track the key exactly.
    /// Shown in every focus mode when something is collapsible (the key fires before the focus match).
    /// Absent for protected-edit warning descriptions.
    #[test]
    fn ctrl_f_hint_follows_the_key_in_every_focus_mode() {
        let mut agent = make_agent();
        agent.pane_areas.prompt = Rect::new(0, 20, 80, 10);
        let mut perm = make_followup_permission_state();
        perm.bash_command_raw = Some(
            (0..12)
                .map(|i| format!("echo line{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        for focus in ALL_FOCUSES {
            perm.focus = focus;
            let hints = agent.permission_shortcut_hints(&perm);
            assert!(
                hints.iter().any(|h| h.label == "expand"),
                "missing Ctrl-F hint in {focus:?}"
            );
        }
        perm.args_expanded = true;
        assert!(
            agent
                .permission_shortcut_hints(&perm)
                .iter()
                .any(|h| h.label == "collapse")
        );
        perm.args_expanded = false;
        perm.bash_command_raw = None;
        perm.description = vec!["Warning: this file is protected".into()];
        perm.options = vec![acp::PermissionOption::new(
            acp::PermissionOptionId::new(Arc::from(
                distill_workspace::permission::ALLOW_EDITS_SESSION_OPTION_ID,
            )),
            "Allow all edits this session".to_owned(),
            acp::PermissionOptionKind::AllowAlways,
        )];
        for focus in ALL_FOCUSES {
            perm.focus = focus;
            let hints = agent.permission_shortcut_hints(&perm);
            assert!(
                !hints
                    .iter()
                    .any(|h| h.label == "expand" || h.label == "collapse"),
                "protected-edit must not advertise Ctrl-F in {focus:?}"
            );
        }
        perm.options.clear();
        perm.description = vec!["{".into(), "  \"k\": 1".into(), "}".into()];
        for focus in ALL_FOCUSES {
            perm.focus = focus;
            let hints = agent.permission_shortcut_hints(&perm);
            assert!(
                hints.iter().any(|h| h.label == "expand"),
                "missing Ctrl-F hint for scope-less MCP args in {focus:?}"
            );
        }
    }
}
#[cfg(test)]
mod status_line_draw_tests {
    use super::super::test_fixtures::make_agent;
    use super::AgentView;
    use crate::actions::ActionRegistry;
    use crate::app::bundle::BundleState;
    use crate::scrollback::render::ScratchBuffer;
    use crate::views::question_view::QuestionViewState;
    use crate::views::status_line::{SanitizedText, StatusLineDisplay, StatusLineFrame};
    use distill_tools::implementations::distill::ask_user_question::{Question, QuestionOption};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    fn draw_script(output: &str, rows: u16) -> Buffer {
        draw_script_for(&mut make_agent(), output, rows)
    }
    fn draw_script_for(agent: &mut AgentView, output: &str, rows: u16) -> Buffer {
        let area = Rect::new(0, 0, 80, rows);
        let mut buf = Buffer::empty(area);
        let mut scratch = ScratchBuffer::new();
        let _ = agent.draw(
            area,
            &mut buf,
            &ActionRegistry::defaults(),
            &mut scratch,
            None,
            false,
            crate::app::agent_view::BannerSlotParams::none(),
            &BundleState::default(),
            false,
            &mut Vec::new(),
            super::AppRenderParams {
                status_line: StatusLineFrame::On {
                    display: std::sync::Arc::new(StatusLineDisplay::Text(SanitizedText::new(
                        output,
                    ))),
                    padding: 0,
                },
                ..Default::default()
            },
        );
        buf
    }
    fn find(buf: &Buffer, text: &str) -> Option<(u16, u16)> {
        let area = *buf.area();
        let want: Vec<String> = text.chars().map(String::from).collect();
        (area.y..area.bottom()).find_map(|y| {
            (area.x..area.right().saturating_sub(want.len() as u16 - 1))
                .find(|x| {
                    want.iter().enumerate().all(|(i, c)| {
                        buf.cell((x + i as u16, y))
                            .is_some_and(|cell| cell.symbol() == c.as_str())
                    })
                })
                .map(|x| (x, y))
        })
    }
    #[test]
    fn script_background_survives_the_pane_fill() {
        let _guard = crate::theme::cache::pin_theme();
        let buf = draw_script("\x1b[41mRED\x1b[0m", 30);
        let (x, y) = find(&buf, "RED").expect("the script row is on screen");
        assert_eq!(buf.cell((x, y)).map(|c| c.bg), Some(Color::Red));
    }
    /// Agent with a many-option question card open, which asks for most of the screen.
    fn question_agent() -> AgentView {
        let mut agent = make_agent();
        let stashed = agent.prompt.stash();
        let options = (0..12)
            .map(|i| QuestionOption {
                label: format!("Option {i}"),
                description: String::new(),
                preview: None,
                id: None,
            })
            .collect();
        let state = QuestionViewState::new(
            "status_line-test".into(),
            vec![Question {
                question: "Pick one of many?".into(),
                options,
                multi_select: Some(false),
                id: None,
            }],
            stashed,
        );
        agent.question_view = Some(state);
        agent
    }
    fn dump(buf: &Buffer) -> String {
        let area = *buf.area();
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_else(|| panic!("missing cell at ({x},{y})"))
                    })
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }
    const FIVE_ROW_SCRIPT: &str = "row-1\nrow-2\nrow-3\nrow-4\nrow-5";
    #[test]
    fn short_terminal_leaves_the_row_four_of_its_five_rows() {
        let buf = draw_script(FIVE_ROW_SCRIPT, 16);
        let screen = dump(&buf);
        assert!(
            find(&buf, "row-4").is_some(),
            "four rows are left over at height 16\n{screen}"
        );
        assert!(
            find(&buf, "row-5").is_none(),
            "a fifth row could only come out of the prompt\n{screen}"
        );
        assert!(
            find(&buf, "\u{2570}").is_some(),
            "the prompt keeps its bottom rule\n{screen}"
        );
        assert!(
            // The label, not the binding: a terminal that cannot send `Ctrl+.` draws `Ctrl+x` for the same cheatsheet
            find(&buf, ":shortcuts").is_some(),
            "the shortcuts bar keeps its row\n{screen}"
        );
    }
    const ONE_ROW_SCRIPT: &str = "solo-row";
    #[test]
    fn fullscreen_question_panel_leaves_the_row_its_rows() {
        let buf = draw_script_for(&mut question_agent(), FIVE_ROW_SCRIPT, 24);
        let screen = dump(&buf);
        let present: Vec<u16> = (1..=5)
            .filter_map(|i| find(&buf, &format!("row-{i}")).map(|(_, y)| y))
            .collect();
        assert_eq!(
            present.len(),
            5,
            "the panel shrinks by the rows the script asks for, got rows at {present:?}\n{screen}"
        );
        assert!(
            find(&buf, "Esc:scrollback").is_some(),
            "the shortcuts bar keeps its row\n{screen}"
        );
    }
    #[test]
    fn fullscreen_question_panel_leaves_a_one_row_script_its_row() {
        let buf = draw_script_for(&mut question_agent(), ONE_ROW_SCRIPT, 24);
        let screen = dump(&buf);
        let row_y = find(&buf, ONE_ROW_SCRIPT)
            .map(|(_, y)| y)
            .unwrap_or_else(|| panic!("the panel must leave the single row on screen\n{screen}"));
        let bar_y = find(&buf, "Esc:scrollback")
            .map(|(_, y)| y)
            .unwrap_or_else(|| panic!("the shortcuts bar keeps its row\n{screen}"));
        assert_eq!(
            bar_y,
            row_y + 1,
            "the shortcuts bar sits directly under the row\n{screen}"
        );
    }
    #[test]
    fn row_clamped_away_by_the_prompt_keeps_the_exported_size() {
        let mut agent = make_agent();
        draw_script_for(&mut agent, ONE_ROW_SCRIPT, 20);
        let painted = agent.last_status_line_size;
        assert_eq!(
            painted,
            Some(crate::views::status_line::RowSize { cols: 76, lines: 1 }),
            "the row fills the inner width of an 80-column area"
        );
        agent.prompt.set_text_preserving(&"line\n".repeat(20));
        let buf = draw_script_for(&mut agent, ONE_ROW_SCRIPT, 20);
        assert!(
            find(&buf, ONE_ROW_SCRIPT).is_none(),
            "a prompt at its cap leaves no row to paint\n{}",
            dump(&buf)
        );
        assert_eq!(
            agent.last_status_line_size, painted,
            "a frame with no row must not export a width the script would read as the 80-column fallback"
        );
    }
}
