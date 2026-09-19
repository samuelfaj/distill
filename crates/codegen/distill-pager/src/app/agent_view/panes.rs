//! Secondary pane input: scrollback keys and search, todo/tool-usage panes, background tasks, subagent catalog, and the pane-aware scroll router.
use super::{ActivePane, AgentPane, AgentView, overlay_action_to_outcome, resolve_action};
use crate::actions::{ActionId, ActionRegistry, When};
use crate::app::actions::Action;
use crate::app::app_view::InputOutcome;
use crate::key;
use crate::scrollback::ScrollbackSearchState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
/// Kill target behind a dock Watchers row.
pub(crate) enum DockWatcherId {
    /// Running `monitor` bg task (bg-task id).
    Monitor(String),
    /// Scheduled `/loop` task (scheduled-task id).
    Loop(String),
}
impl AgentView {
    /// Scrollback-focused key handling.
    /// When the block viewer is open, routes keys to the viewer.
    /// Otherwise, uses ActionRegistry for keybinding lookup.
    pub(super) fn handle_scrollback_key(
        &mut self,
        key: &KeyEvent,
        registry: &ActionRegistry,
    ) -> InputOutcome {
        if let Some(outcome) = self.handle_scrollback_search_key(key) {
            return outcome;
        }
        let viewer_has_input = self
            .block_viewer
            .as_ref()
            .is_some_and(|v| v.list_state.input_mode().is_some());
        let allow_i_alt = self.vim_mode;
        if !viewer_has_input
            && (matches!(key.code, KeyCode::Tab | KeyCode::Char(' '))
                || (allow_i_alt && matches!(key.code, KeyCode::Char('i'))))
        {
            if self.parked_card().is_some() {
                self.set_active_pane(AgentPane::Prompt, false);
                return InputOutcome::Changed;
            }
            if key.code == KeyCode::Tab
                && self.dock_shown
                && !self.dock_hidden
                && self.set_active_pane(AgentPane::Dock, false)
            {
                self.dock_cursor = 0;
                return InputOutcome::Changed;
            }
            if key.code == KeyCode::Tab
                && self.tasks.overlay.visible
                && self.set_active_pane(AgentPane::Tasks, false)
            {
                self.tasks.overlay.focused = true;
                return InputOutcome::Changed;
            }
            return InputOutcome::Action(Action::FocusPrompt);
        }
        if key!(Enter).matches(key)
            && let Some(target) = self.highlighted_link_target().cloned()
        {
            self.highlighted_link_idx = None;
            return InputOutcome::Action(Action::OpenLink(target));
        }
        if crate::app::inline_edit::INLINE_EDIT_ENABLED
            && key!(Enter).matches(key)
            && !self.scrollback.is_selected_group_header()
            && let Some(idx) = self.scrollback.selected()
            && self
                .scrollback
                .entry(idx)
                .is_some_and(|e| e.block.is_user_prompt())
            && self.enter_inline_edit(idx)
        {
            return InputOutcome::Changed;
        }
        let action = registry.lookup_with_mode(key, When::ScrollbackFocused, self.vim_mode);
        if action == Some(ActionId::OpenBlockViewer) && self.try_open_child_from_selected_row() {
            return InputOutcome::Changed;
        }
        if self.vim_mode
            && key!('x').matches(key)
            && !self.scrollback.is_selected_group_header()
            && let Some(idx) = self.scrollback.selected()
            && let Some(entry) = self.scrollback.entry(idx)
            && let crate::scrollback::block::RenderBlock::BgTask(ref bt) = entry.block
            && self
                .session
                .bg_tasks
                .get(&bt.task_id)
                .is_some_and(|t| t.status == crate::app::agent::BgTaskStatus::Running)
        {
            return InputOutcome::Action(Action::KillBgTask(bt.task_id.clone()));
        }
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.persistent_text_selection.take().is_some()
        {
            self.table_selection_geometry = None;
            self.selection_created_at = None;
            return InputOutcome::Changed;
        }
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.highlighted_link_idx.take().is_some()
        {
            return InputOutcome::Changed;
        }
        if self.vim_mode
            && key!('/').matches(key)
            && self.no_input_overlay_pending()
            && self.btw_state.is_none()
        {
            if self.scrollback.is_empty() {
                return InputOutcome::ActionThenForward(Action::FocusPrompt);
            }
            self.open_scrollback_search(None);
            return InputOutcome::Changed;
        }
        if registry.lookup(key, When::ScrollbackFocused) == Some(ActionId::ToggleMouseCapture) {
            return InputOutcome::Action(Action::ToggleMouseCapture);
        }
        if let Some(outcome) = resolve_action(action) {
            return outcome;
        }
        if !self.vim_mode
            && let KeyCode::Char(c) = key.code
            && (c.is_ascii_alphabetic() || c == '/')
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            return InputOutcome::ActionThenForward(Action::FocusPrompt);
        }
        InputOutcome::Unchanged
    }
    /// Focus the scrollback pane and open an incremental search over it.
    /// Opens only if the pane switch succeeds: a dirty queued-prompt edit blocks the switch, and the search bar only works with scrollback focused.
    /// `initial_query` (the `/find <word>` argument) is fed through the keystroke path, so a pre-filled search behaves like typing into the bar.
    pub(crate) fn open_scrollback_search(&mut self, initial_query: Option<&str>) {
        if self.set_active_pane(AgentPane::Scrollback, false) {
            self.scrollback_search = Some(ScrollbackSearchState::open());
            if let Some(query) = initial_query {
                self.set_scrollback_search_query(query);
            }
        }
    }
    /// Step to the next (`forward`) or previous match and scroll it into view.
    /// Both the `n`/`N` keys and the Down/Up arrows land here.
    fn navigate_search(&mut self, forward: bool) -> Option<InputOutcome> {
        if let Some(search) = self.scrollback_search.as_mut() {
            if forward {
                search.next();
            } else {
                search.prev();
            }
        }
        self.reveal_current_search_match();
        Some(InputOutcome::Changed)
    }
    /// Bottom scrollback rows to reserve for the search UI (divider and bar): two when search is active.
    /// The count clamps to the rows that actually exist, so a very short region never pushes the bar below the scrollback rect.
    pub(super) fn search_reserved_rows(scrollback_height: u16, search_active: bool) -> u16 {
        if search_active {
            scrollback_height.min(2)
        } else {
            0
        }
    }
    /// Handle a key while the scrollback search overlay is open.
    /// Returns `None` when search isn't open (or, while browsing, for keys that should fall through to normal scrollback handling).
    /// While composing the query the bar is modal and swallows other keys.
    fn handle_scrollback_search_key(&mut self, key: &KeyEvent) -> Option<InputOutcome> {
        let composing = self.scrollback_search.as_ref()?.is_composing();
        let non_text = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
        if key.code == KeyCode::Esc {
            self.scrollback_search = None;
            return Some(InputOutcome::Changed);
        }
        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Down => return self.navigate_search(true),
                KeyCode::Up => return self.navigate_search(false),
                _ => {}
            }
        }
        if composing {
            if key.code == KeyCode::Enter {
                if self.scrollback_search.as_ref()?.query().is_empty() {
                    self.scrollback_search = None;
                } else {
                    if let Some(search) = self.scrollback_search.as_mut() {
                        search.accept();
                    }
                    self.reveal_current_search_match();
                }
                return Some(InputOutcome::Changed);
            }
            let outcome = self
                .scrollback_search
                .as_mut()?
                .apply_query_key(key, &self.scrollback);
            Some(match outcome {
                crate::input::line_editor::LineEditOutcome::TextChanged
                | crate::input::line_editor::LineEditOutcome::CursorChanged
                | crate::input::line_editor::LineEditOutcome::HandledNoChange => {
                    InputOutcome::Changed
                }
                crate::input::line_editor::LineEditOutcome::Unhandled => InputOutcome::Unchanged,
            })
        } else {
            match key.code {
                KeyCode::Char('n') if key.modifiers.is_empty() => self.navigate_search(true),
                KeyCode::Char('N') if !key.modifiers.intersects(non_text) => {
                    self.navigate_search(false)
                }
                _ => None,
            }
        }
    }
    pub(super) fn handle_scrollback_search_paste(&mut self, text: &str) -> Option<InputOutcome> {
        let search = self.scrollback_search.as_mut()?;
        if !search.is_composing() {
            return Some(InputOutcome::Unchanged);
        }
        let outcome = search.apply_query_paste(text, &self.scrollback);
        Some(match outcome {
            crate::input::line_editor::LineEditOutcome::TextChanged
            | crate::input::line_editor::LineEditOutcome::CursorChanged
            | crate::input::line_editor::LineEditOutcome::HandledNoChange => InputOutcome::Changed,
            crate::input::line_editor::LineEditOutcome::Unhandled => InputOutcome::Unchanged,
        })
    }
    /// Enqueue `query` for the background scan.
    /// Results (and the reveal) arrive later via [`poll_scrollback_search`](Self::poll_scrollback_search).
    /// The highlight updates immediately because it reads the UI-side matcher.
    fn set_scrollback_search_query(&mut self, query: &str) {
        if let Some(search) = self.scrollback_search.as_mut() {
            search.update_query(query, &self.scrollback);
        }
    }
    /// Poll the background search daemon for new results, revealing the freshly parked match when they change.
    /// Returns `true` if the UI should redraw.
    pub(crate) fn poll_scrollback_search(&mut self) -> bool {
        let changed = self.scrollback_search.as_mut().is_some_and(|s| s.poll());
        if changed {
            self.reveal_current_search_match();
        }
        changed
    }
    /// Scroll the current search match into view via `reveal_entry_line`.
    fn reveal_current_search_match(&mut self) {
        let target = self
            .scrollback_search
            .as_ref()
            .and_then(|s| s.current())
            .map(|m| (m.entry_id, m.line_in_entry));
        if let Some((id, line)) = target
            && let Some(idx) = self.scrollback.index_of_id(id)
        {
            self.scrollback.reveal_entry_line(idx, line);
        }
    }
    /// Todo-pane-focused key handling.
    ///
    /// Routes structural keys through the shared overlay handler, then content keys through `TodoPane::handle_key`.
    pub(super) fn handle_todo_key(
        &mut self,
        key: &KeyEvent,
        _registry: &ActionRegistry,
    ) -> InputOutcome {
        use crate::views::overlay::{handle_overlay_key, handle_overlay_nav_key};
        if key!('t', CONTROL).matches(key) {
            self.todo.overlay.toggle();
            self.todo.on_state_change();
            if !self.todo.overlay.focused {
                return InputOutcome::Action(Action::FocusScrollback);
            }
            return InputOutcome::Changed;
        }
        let has_input = self.todo.list_state.input_mode().is_some();
        let action = handle_overlay_key(&mut self.todo.overlay, key).or_else(|| {
            if !has_input {
                handle_overlay_nav_key(&mut self.todo.overlay, key)
            } else {
                None
            }
        });
        if let Some(action) = action {
            self.todo.on_state_change();
            if !self.todo.overlay.visible || !self.todo.overlay.focused {
                self.set_active_pane(AgentPane::Scrollback, false);
            }
            return overlay_action_to_outcome(action);
        }
        if self.todo.handle_key(key) {
            InputOutcome::Changed
        } else {
            InputOutcome::Unchanged
        }
    }
    pub(crate) fn dock_workflow_rows(&self) -> Vec<(String, crate::views::dock::DockRow)> {
        self.workflow_runs_newest_first()
            .into_iter()
            .filter(|run| !run.is_terminal())
            .map(|run| {
                let activity = run.activity_label();
                (
                    run.run_id.clone(),
                    crate::views::dock::DockRow {
                        kind: "Workflow".into(),
                        description: run.name.clone(),
                        activity: (!activity.is_empty()).then_some(activity),
                        meta: crate::views::dock::fmt_elapsed(run.live_elapsed_ms() / 1000),
                        killable: run.can_stop(),
                        openable: true,
                        spinning: run.is_active(),
                    },
                )
            })
            .collect()
    }
    /// Non-workflow subagents in dock display order: `(child_session_id, subagent_id, row)`.
    pub(crate) fn dock_subagent_rows(&self) -> Vec<(String, String, crate::views::dock::DockRow)> {
        let show_done = self.tasks.show_done();
        let mut infos: Vec<&crate::app::subagent::SubagentInfo> = self
            .subagent_sessions
            .values()
            .filter(|info| info.attempt.workflow_run_id.is_none())
            .filter(|info| show_done || info.is_running())
            .collect();
        infos.sort_by_key(|info| (!info.is_running(), info.attempt.started_at));
        infos
            .into_iter()
            .map(|info| {
                let running = info.is_running();
                let (kind, description) = crate::app::subagent::format_subagent_label(info);
                let mut meta = String::new();
                if let Some(model) = info.attempt.model.as_deref().filter(|s| !s.is_empty()) {
                    meta.push_str(model);
                    meta.push(' ');
                }
                meta.push_str(&crate::views::dock::fmt_elapsed(
                    info.display_elapsed().as_secs(),
                ));
                (
                    info.child_session_id.to_string(),
                    info.subagent_id.to_string(),
                    crate::views::dock::DockRow {
                        kind,
                        description,
                        activity: if running {
                            info.attempt.activity_label.clone()
                        } else {
                            info.attempt.status.as_deref().map(str::to_owned)
                        },
                        meta,
                        killable: running && !info.attempt.pending_kill,
                        openable: true,
                        spinning: running,
                    },
                )
            })
            .collect()
    }
    /// Running background commands (non-monitor): `(task_id, row)`.
    pub(crate) fn dock_task_rows(&self) -> Vec<(String, crate::views::dock::DockRow)> {
        let mut tasks: Vec<&crate::app::agent::BgTaskState> = self
            .session
            .bg_tasks
            .values()
            .filter(|t| t.status == crate::app::agent::BgTaskStatus::Running && !t.is_monitor)
            .collect();
        tasks.sort_by_key(|t| t.start_time);
        tasks
            .into_iter()
            .map(|t| {
                let description = t
                    .description
                    .clone()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| t.command.clone());
                let elapsed = t.start_time.elapsed().unwrap_or_default().as_secs();
                (
                    t.task_id.clone(),
                    crate::views::dock::DockRow {
                        kind: "Run".into(),
                        description,
                        activity: None,
                        meta: crate::views::dock::fmt_elapsed(elapsed),
                        killable: !t.pending_kill,
                        openable: true,
                        spinning: true,
                    },
                )
            })
            .collect()
    }
    /// Running monitors, then scheduled loops, in dock display order.
    pub(crate) fn dock_watcher_rows(&self) -> Vec<(DockWatcherId, crate::views::dock::DockRow)> {
        let mut monitors: Vec<&crate::app::agent::BgTaskState> = self
            .session
            .bg_tasks
            .values()
            .filter(|t| t.status == crate::app::agent::BgTaskStatus::Running && t.is_monitor)
            .collect();
        monitors.sort_by_key(|t| t.start_time);
        let mut rows: Vec<(DockWatcherId, crate::views::dock::DockRow)> = monitors
            .into_iter()
            .map(|t| {
                let description = t
                    .description
                    .clone()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| t.command.clone());
                let elapsed = t.start_time.elapsed().unwrap_or_default().as_secs();
                (
                    DockWatcherId::Monitor(t.task_id.clone()),
                    crate::views::dock::DockRow {
                        kind: "Monitor".into(),
                        description,
                        activity: None,
                        meta: crate::views::dock::fmt_elapsed(elapsed),
                        killable: !t.pending_kill,
                        openable: true,
                        spinning: true,
                    },
                )
            })
            .collect();
        let mut loops: Vec<&crate::app::agent::ScheduledTaskInfo> =
            self.session.scheduled_tasks.values().collect();
        loops.sort_by_key(|s| s.created_at);
        let now = chrono::Utc::now();
        rows.extend(loops.into_iter().map(|s| {
            (
                DockWatcherId::Loop(s.task_id.clone()),
                crate::views::dock::DockRow {
                    kind: "Loop".into(),
                    description: s.prompt.clone(),
                    activity: None,
                    meta: format!(
                        "{}{}",
                        s.human_schedule,
                        crate::views::scheduled_next::next_suffix(s, now)
                    ),
                    killable: true,
                    openable: s.last_subagent_id.as_deref().is_some_and(|sid| {
                        self.subagent_sessions.iter().any(|(child, info)| {
                            info.subagent_id.as_ref() == sid
                                && self.subagent_views.contains_key(child)
                        })
                    }),
                    spinning: false,
                },
            )
        }));
        rows
    }
    /// The counts the dock lays out from, carrying the ceiling every consumer obeys.
    pub(crate) fn dock_counts(&self) -> crate::views::dock::DockCounts {
        crate::views::dock::DockCounts {
            workflows: self.dock_workflow_rows().len(),
            subagents: self.dock_subagent_rows().len(),
            tasks: self.dock_task_rows().len(),
            watchers: self.dock_watcher_rows().len(),
            queued: self.visible_held_queue_len(),
            workflows_expanded: self.dock_workflows_expanded,
            subagents_expanded: self.dock_subagents_expanded,
            tasks_expanded: self.dock_tasks_expanded,
            watchers_expanded: self.dock_watchers_expanded,
            workflows_show_all: self.dock_workflows_show_all,
            subagents_show_all: self.dock_subagents_show_all,
            tasks_show_all: self.dock_tasks_show_all,
            watchers_show_all: self.dock_watchers_show_all,
            queue_body_rows: self.queue.desired_height(),
            offsets: self.dock_offsets,
            max_rows: self.dock_max_rows(),
        }
    }
    /// Rows the dock may take: its resting height, raised for a revealed section
    /// to half the space above the prompt. `paint_cap` stretches this when the
    /// floors need more, and the frame's layout has the last word.
    fn dock_max_rows(&self) -> crate::views::dock::MaxRows {
        use crate::views::dock::{MAX_DOCK_ROWS, MaxRows};
        let above_prompt = self
            .pane_areas
            .scrollback
            .height
            .saturating_add(self.pane_areas.dock.height);
        let ceiling = above_prompt
            .saturating_sub(crate::views::agent::SCROLLBACK_MIN_ROWS)
            .max(MAX_DOCK_ROWS);
        let revealed = self.dock_workflows_show_all
            || self.dock_subagents_show_all
            || self.dock_tasks_show_all
            || self.dock_watchers_show_all;
        if revealed {
            MaxRows::new(ceiling)
        } else {
            MaxRows::default()
        }
    }
    /// Item painted at `row` on screen, or `None` for the queue body and rows past the content.
    pub(crate) fn dock_item_at(
        &self,
        dock: ratatui::layout::Rect,
        row: u16,
    ) -> Option<crate::views::dock::DockItem> {
        self.dock_layout_for(dock.height)
            .item_at(row.checked_sub(dock.y)?)
    }
    /// Spends the pending reveal. Every path that ends a frame calls this, so a takeover that never paints the dock cannot leave it budgeting rows nothing put on screen.
    /// takeover that never paints the dock cannot leave it budgeting rows nothing
    /// put on screen.
    pub(crate) fn take_dock_row_request(&mut self) -> bool {
        std::mem::take(&mut self.dock_reveal_pending)
    }
    /// Keyboard, wheel, and clamp budget the same rows paint used. A reveal
    /// budgets against the rows it asked for until the frame assigns them, so the
    /// cursor can reach a row it just uncovered.
    pub(crate) fn dock_layout(&self) -> crate::views::dock::DockLayout {
        let assigned = self.pane_areas.dock.height;
        if self.dock_reveal_pending || assigned == 0 {
            crate::views::dock::DockLayout::new(&self.dock_counts())
        } else {
            self.dock_layout_for(assigned)
        }
    }
    /// Budget rows against `height`, not `dock_max_rows` / a prior frame's
    /// `pane_areas.dock`. Paint, hover, and the wheel all pass this frame's
    /// assigned rect so they cannot drift from what is on screen.
    fn dock_layout_for(&self, height: u16) -> crate::views::dock::DockLayout {
        let counts = self.dock_counts();
        match height {
            0 => crate::views::dock::DockLayout::new(&counts),
            assigned => crate::views::dock::DockLayout::with_cap(&counts, assigned as usize),
        }
    }
    pub(crate) fn is_dock_section_expanded(&self, section: crate::views::dock::Section) -> bool {
        match section {
            crate::views::dock::Section::Workflows => self.dock_workflows_expanded,
            crate::views::dock::Section::Subagents => self.dock_subagents_expanded,
            crate::views::dock::Section::Tasks => self.dock_tasks_expanded,
            crate::views::dock::Section::Watchers => self.dock_watchers_expanded,
            crate::views::dock::Section::Queued => self.dock_queued_expanded,
        }
    }
    fn set_dock_section_expanded(
        &mut self,
        section: crate::views::dock::Section,
        expanded: bool,
    ) -> InputOutcome {
        let slot = match section {
            crate::views::dock::Section::Workflows => &mut self.dock_workflows_expanded,
            crate::views::dock::Section::Subagents => &mut self.dock_subagents_expanded,
            crate::views::dock::Section::Tasks => &mut self.dock_tasks_expanded,
            crate::views::dock::Section::Watchers => &mut self.dock_watchers_expanded,
            crate::views::dock::Section::Queued => &mut self.dock_queued_expanded,
        };
        if *slot == expanded {
            return InputOutcome::Unchanged;
        }
        *slot = expanded;
        if !expanded {
            match section {
                crate::views::dock::Section::Workflows => {
                    self.dock_workflows_show_all = false;
                }
                crate::views::dock::Section::Subagents => {
                    self.dock_subagents_show_all = false;
                }
                crate::views::dock::Section::Tasks => self.dock_tasks_show_all = false,
                crate::views::dock::Section::Watchers => {
                    self.dock_watchers_show_all = false;
                }
                crate::views::dock::Section::Queued => {}
            }
        }
        InputOutcome::Changed
    }
    /// Extra rows belong to one section. Revealing Watchers must drop Tasks'
    /// raise so the two do not share the lift and shrink each other.
    fn set_dock_section_show_all(&mut self, section: crate::views::dock::Section) {
        match section {
            crate::views::dock::Section::Workflows => self.dock_workflows_show_all = true,
            crate::views::dock::Section::Subagents => self.dock_subagents_show_all = true,
            crate::views::dock::Section::Tasks => self.dock_tasks_show_all = true,
            crate::views::dock::Section::Watchers => self.dock_watchers_show_all = true,
            crate::views::dock::Section::Queued => return,
        }
        self.dock_reveal_pending = true;
    }
    pub(super) fn clamp_dock_overflow(&mut self) {
        use crate::views::dock::{Section, is_show_all_needed};
        let counts = self.dock_counts();
        if !is_show_all_needed(&counts, Section::Workflows) {
            self.dock_workflows_show_all = false;
        }
        if !is_show_all_needed(&counts, Section::Subagents) {
            self.dock_subagents_show_all = false;
        }
        if !is_show_all_needed(&counts, Section::Tasks) {
            self.dock_tasks_show_all = false;
        }
        if !is_show_all_needed(&counts, Section::Watchers) {
            self.dock_watchers_show_all = false;
        }
        let layout = self.dock_layout();
        for section in [
            Section::Workflows,
            Section::Subagents,
            Section::Tasks,
            Section::Watchers,
        ] {
            self.dock_offsets.set(section, layout.row_offset(section));
        }
        let n = self.dock_items().len();
        self.dock_cursor = if n == 0 {
            0
        } else {
            self.dock_cursor.min(n - 1)
        };
    }
    /// Pre-paint dock reconciliation, run from the draw state-update step rather than the paint pass. Section counts change from background events (task and subagent completion, queue refills), so a section that shrank back to the preview must drop its `show-all` and the cursor must stay in bounds even when no dock key was pressed since the change. Gated on the prior frame's `dock_on` so it does no work while the dock is off.
    pub(crate) fn reconcile_dock_before_paint(&mut self) {
        if self.dock_on {
            self.clamp_dock_overflow();
        }
    }
    pub(crate) fn dock_enter_label(&self) -> Option<&'static str> {
        use crate::views::dock::{DockItem, Section};
        match self.dock_items().get(self.dock_cursor) {
            Some(DockItem::Header(sec)) => Some(if self.is_dock_section_expanded(*sec) {
                "collapse"
            } else {
                "expand"
            }),
            Some(DockItem::RevealRemaining(_)) => Some("show all"),
            Some(DockItem::Row(section, i)) => {
                let openable = match section {
                    Section::Workflows => self
                        .dock_workflow_rows()
                        .get(*i)
                        .is_some_and(|(_, row)| row.openable),
                    Section::Subagents => self
                        .dock_subagent_rows()
                        .get(*i)
                        .is_some_and(|(_, _, row)| row.openable),
                    Section::Tasks => self
                        .dock_task_rows()
                        .get(*i)
                        .is_some_and(|(_, row)| row.openable),
                    Section::Watchers => self
                        .dock_watcher_rows()
                        .get(*i)
                        .is_some_and(|(_, row)| row.openable),
                    Section::Queued => true,
                };
                openable.then_some("open")
            }
            _ => None,
        }
    }
    pub(crate) fn dock_snapshot(&self) -> crate::views::dock::DockData {
        crate::views::dock::DockData {
            workflows: self
                .dock_workflow_rows()
                .into_iter()
                .map(|(_, row)| row)
                .collect(),
            subagents: self
                .dock_subagent_rows()
                .into_iter()
                .map(|(_, _, row)| row)
                .collect(),
            tasks: self
                .dock_task_rows()
                .into_iter()
                .map(|(_, row)| row)
                .collect(),
            watchers: self
                .dock_watcher_rows()
                .into_iter()
                .map(|(_, row)| row)
                .collect(),
            queued: self.visible_held_queue_len(),
            workflows_expanded: self.dock_workflows_expanded,
            subagents_expanded: self.dock_subagents_expanded,
            tasks_expanded: self.dock_tasks_expanded,
            watchers_expanded: self.dock_watchers_expanded,
            workflows_show_all: self.dock_workflows_show_all,
            subagents_show_all: self.dock_subagents_show_all,
            tasks_show_all: self.dock_tasks_show_all,
            watchers_show_all: self.dock_watchers_show_all,
            focused: self.active_pane == ActivePane::Dock,
            cursor: self.dock_cursor,
            queue_body_rows: self.queue.desired_height(),
            offsets: self.dock_offsets,
            max_rows: self.dock_max_rows(),
            hovered: self.dock_hovered,
            stop_hovered: false,
            spinner_tick: self.tasks.tick_count(),
        }
    }
    pub(crate) fn cache_dock_stop_at(
        &mut self,
        area: ratatui::layout::Rect,
        data: &crate::views::dock::DockData,
    ) {
        self.dock_stop_button =
            crate::views::dock::hovered_stop_button_rect(area, data).and_then(|hit| {
                self.dock_stop_action(hit.item)
                    .and_then(|action| super::DockKillId::from_action(&action))
                    .map(|id| super::CachedDockStop { rect: hit.rect, id })
            });
    }
    pub(crate) fn cache_dock_stop_button(&mut self) {
        let snapshot = self.dock_snapshot();
        self.cache_dock_stop_at(self.pane_areas.dock, &snapshot);
    }
    /// Re-derive `dock_hovered` from the last pointer position against `dock`
    /// (this frame's dock rect) and the current viewport. Pass the live
    /// `layout.dock` rather than reading `pane_areas.dock`, which still holds the previous frame's rect during paint.
    pub(crate) fn sync_dock_hover_from_pointer(&mut self, dock: ratatui::layout::Rect) {
        let (col, row) = self.last_mouse_pos;
        self.dock_hovered = dock
            .contains((col, row).into())
            .then(|| self.dock_item_at(dock, row))
            .flatten();
    }
    /// Headers stay put, so scrolling a section can never push another off the dock.
    pub(crate) fn scroll_dock_section(
        &mut self,
        section: crate::views::dock::Section,
        delta: isize,
    ) -> bool {
        let layout = self.dock_layout();
        if layout.visible_rows(section) == 0 {
            return false;
        }
        let current = layout.row_offset(section);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current + (delta as usize).min(layout.rows_below(section))
        };
        if next == current && next == self.dock_offsets.get(section) {
            return false;
        }
        self.dock_offsets.set(section, next);
        true
    }
    pub(crate) fn dock_section_at(
        &self,
        dock: ratatui::layout::Rect,
        row: u16,
    ) -> Option<crate::views::dock::Section> {
        match self.dock_item_at(dock, row)? {
            crate::views::dock::DockItem::Header(section)
            | crate::views::dock::DockItem::Row(section, _)
            | crate::views::dock::DockItem::RevealRemaining(section) => Some(section),
        }
    }
    pub(crate) fn dock_items(&self) -> Vec<crate::views::dock::DockItem> {
        self.dock_layout().rows().to_vec()
    }
    pub(crate) fn dock_stop_action(&self, item: crate::views::dock::DockItem) -> Option<Action> {
        use crate::views::dock::{DockItem, Section};
        match item {
            DockItem::Row(Section::Workflows, i) => {
                self.dock_workflow_rows().get(i).and_then(|(_, row)| {
                    row.killable.then(|| {
                        Action::SendSlashCommandPreservingDraft(format!(
                            "/workflow stop {}",
                            row.description
                        ))
                    })
                })
            }
            DockItem::Row(Section::Subagents, i) => self
                .dock_subagent_rows()
                .get(i)
                .filter(|(_, _, row)| row.killable)
                .map(|(_, subagent_id, _)| Action::KillSubagent(subagent_id.clone())),
            DockItem::Row(Section::Tasks, i) => self
                .dock_task_rows()
                .get(i)
                .filter(|(_, row)| row.killable)
                .map(|(task_id, _)| Action::KillBgTask(task_id.clone())),
            DockItem::Row(Section::Watchers, i) => {
                self.dock_watcher_rows().get(i).and_then(|(id, row)| {
                    row.killable.then(|| match id {
                        DockWatcherId::Monitor(task_id) => Action::KillBgTask(task_id.clone()),
                        DockWatcherId::Loop(task_id) => {
                            Action::CancelScheduledTask(task_id.clone())
                        }
                    })
                })
            }
            DockItem::Row(Section::Queued, _)
            | DockItem::Header(_)
            | DockItem::RevealRemaining(_) => None,
        }
    }
    pub(crate) fn activate_dock_item(
        &mut self,
        item: crate::views::dock::DockItem,
    ) -> InputOutcome {
        use crate::views::dock::{DockItem, Section};
        match item {
            DockItem::Header(sec) => {
                let next = !self.is_dock_section_expanded(sec);
                self.set_dock_section_expanded(sec, next)
            }
            DockItem::Row(Section::Workflows, i) => {
                if let Some((run_id, _)) = self.dock_workflow_rows().get(i) {
                    let run_id = run_id.clone();
                    self.open_workflow_detail_by_run_id(&run_id);
                    InputOutcome::Changed
                } else {
                    InputOutcome::Unchanged
                }
            }
            DockItem::Row(Section::Subagents, i) => {
                if let Some((child_sid, _, row)) = self.dock_subagent_rows().get(i) {
                    if !row.openable || child_sid.is_empty() {
                        return InputOutcome::Unchanged;
                    }
                    let sid = child_sid.clone();
                    self.open_subagent_fullscreen(sid);
                    InputOutcome::Changed
                } else {
                    InputOutcome::Unchanged
                }
            }
            DockItem::Row(Section::Tasks, i) => self
                .dock_task_rows()
                .get(i)
                .map(|(id, _)| id.clone())
                .map_or(InputOutcome::Unchanged, |id| self.open_bg_task_viewer(&id)),
            DockItem::Row(Section::Watchers, i) => match self.dock_watcher_rows().get(i) {
                Some((DockWatcherId::Monitor(id), _)) => {
                    let id = id.clone();
                    self.open_bg_task_viewer(&id)
                }
                Some((DockWatcherId::Loop(id), _)) => {
                    let id = id.clone();
                    self.open_linked_scheduled_subagent(&id)
                }
                None => InputOutcome::Unchanged,
            },
            DockItem::RevealRemaining(section) => {
                let layout = self.dock_layout();
                let first_hidden = layout.row_offset(section) + layout.visible_rows(section);
                if section == Section::Queued {
                    return InputOutcome::Unchanged;
                }
                self.set_dock_section_show_all(section);
                if let Some(next) = self
                    .dock_items()
                    .iter()
                    .position(|item| *item == DockItem::Row(section, first_hidden))
                {
                    self.dock_cursor = next;
                }
                InputOutcome::Changed
            }
            DockItem::Row(Section::Queued, _) => InputOutcome::Unchanged,
        }
    }
    fn open_bg_task_viewer(&mut self, task_id: &str) -> InputOutcome {
        if self.show_bg_task_viewer(task_id) {
            InputOutcome::Changed
        } else {
            InputOutcome::Unchanged
        }
    }
    fn open_linked_scheduled_subagent(&mut self, task_id: &str) -> InputOutcome {
        let Some(child_sid) = self
            .session
            .scheduled_tasks
            .get(task_id)
            .and_then(|info| info.last_subagent_id.as_deref())
            .and_then(|sid| {
                self.subagent_sessions.iter().find_map(|(child, info)| {
                    (info.subagent_id.as_ref() == sid).then_some(child.clone())
                })
            })
            .filter(|child| self.subagent_views.contains_key(child))
        else {
            return InputOutcome::Unchanged;
        };
        self.open_subagent_fullscreen(child_sid);
        InputOutcome::Changed
    }
    /// Dock-focused key handling (remote `dock_enabled`).
    pub(super) fn handle_dock_key(&mut self, key: &KeyEvent) -> InputOutcome {
        use crate::views::dock::DockItem;
        use crossterm::event::KeyCode;
        if !self.dock_shown || self.dock_hidden {
            return InputOutcome::Unchanged;
        }
        let shift_tab = matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && key.modifiers == KeyModifiers::SHIFT;
        if !key.modifiers.is_empty() && !shift_tab {
            return InputOutcome::Unchanged;
        }
        self.clamp_dock_overflow();
        let items = self.dock_items();
        if items.is_empty() {
            self.set_active_pane(AgentPane::Scrollback, false);
            return InputOutcome::Changed;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.dock_cursor = self.dock_cursor.saturating_sub(1);
                InputOutcome::Changed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.dock_cursor = (self.dock_cursor + 1).min(items.len() - 1);
                InputOutcome::Changed
            }
            KeyCode::Right | KeyCode::Char('l') => match items.get(self.dock_cursor) {
                Some(DockItem::Header(sec)) => self.set_dock_section_expanded(*sec, true),
                Some(DockItem::Row(..) | DockItem::RevealRemaining(_)) | None => {
                    InputOutcome::Unchanged
                }
            },
            KeyCode::Left => match items.get(self.dock_cursor) {
                Some(DockItem::Header(sec)) => self.set_dock_section_expanded(*sec, false),
                Some(DockItem::Row(..) | DockItem::RevealRemaining(_)) | None => {
                    InputOutcome::Unchanged
                }
            },
            KeyCode::Char('h') => {
                self.tasks.toggle_show_done();
                if self.tasks.show_done() {
                    self.dock_subagents_expanded = true;
                }
                self.clamp_dock_overflow();
                InputOutcome::Changed
            }
            KeyCode::Tab if !shift_tab => InputOutcome::Action(Action::FocusPrompt),
            KeyCode::BackTab | KeyCode::Tab => {
                self.set_active_pane(AgentPane::Scrollback, false);
                InputOutcome::Changed
            }
            KeyCode::Enter | KeyCode::Char(' ') => items
                .get(self.dock_cursor)
                .copied()
                .map_or(InputOutcome::Unchanged, |item| {
                    self.activate_dock_item(item)
                }),
            KeyCode::Char('x') => items
                .get(self.dock_cursor)
                .copied()
                .and_then(|item| self.dock_stop_action(item))
                .map_or(InputOutcome::Unchanged, InputOutcome::Action),
            KeyCode::Esc | KeyCode::Char('q') => {
                self.set_active_pane(AgentPane::Scrollback, false);
                InputOutcome::Changed
            }
            _ => InputOutcome::Unchanged,
        }
    }
    /// Bg-task-pane-focused key handling.
    pub(super) fn handle_bg_tasks_key(
        &mut self,
        key: &KeyEvent,
        registry: &ActionRegistry,
    ) -> InputOutcome {
        use crate::views::overlay::{handle_overlay_key, handle_overlay_nav_key};
        use crate::views::tasks_pane::TaskEntry;
        if registry.matches_id(ActionId::ToggleTasks, key) {
            self.tasks.overlay.toggle();
            self.tasks.on_state_change();
            if !self.tasks.overlay.focused {
                return InputOutcome::Action(Action::FocusScrollback);
            }
            return InputOutcome::Changed;
        }
        if self.tasks.list_state.input_mode().is_none()
            && let Some(group) = self.tasks.selected_header_group()
        {
            if key!(Right).matches(key) {
                self.tasks.set_group_collapsed(group, false);
                return InputOutcome::Changed;
            }
            if key!(Left).matches(key) {
                self.tasks.set_group_collapsed(group, true);
                return InputOutcome::Changed;
            }
        }
        let is_open_key = self.tasks.list_state.input_mode().is_none()
            && (key!(Enter).matches(key) || key!('f', CONTROL).matches(key));
        if is_open_key {
            if let Some(group) = self.tasks.selected_header_group() {
                self.tasks.toggle_group(group);
                return InputOutcome::Changed;
            }
            match self.tasks.selected_entry() {
                Some(TaskEntry::BgTask { task_id, .. }) => {
                    let task_id = task_id.clone();
                    if self.show_bg_task_viewer(&task_id) {
                        return InputOutcome::Changed;
                    }
                }
                Some(TaskEntry::Agent {
                    child_session_id, ..
                }) => {
                    let child_sid = child_session_id.clone();
                    if self.subagent_views.contains_key(&child_sid) {
                        self.open_subagent_fullscreen(child_sid);
                        return InputOutcome::Changed;
                    }
                }
                Some(TaskEntry::Scheduled { .. }) => {}
                Some(TaskEntry::Workflow { name, .. }) => {
                    let name = name.clone();
                    self.open_workflow_detail(&name);
                    return InputOutcome::Changed;
                }
                Some(TaskEntry::Header { .. }) => {}
                None => {}
            }
        }
        if key!('x').matches(key) && self.tasks.list_state.input_mode().is_none() {
            match self.tasks.selected_entry() {
                Some(TaskEntry::BgTask { task_id, .. }) => {
                    let task_id = task_id.clone();
                    if self
                        .session
                        .bg_tasks
                        .get(&task_id)
                        .is_some_and(|t| t.status == crate::app::agent::BgTaskStatus::Running)
                    {
                        return InputOutcome::Action(Action::KillBgTask(task_id));
                    }
                }
                Some(TaskEntry::Agent { subagent_id, .. }) => {
                    let subagent_id = subagent_id.clone();
                    if self.subagent_sessions.values().any(|s| {
                        s.subagent_id.as_ref() == subagent_id
                            && s.is_running()
                            && !s.attempt.pending_kill
                    }) {
                        return InputOutcome::Action(Action::KillSubagent(subagent_id));
                    }
                }
                Some(TaskEntry::Scheduled { task_id, .. }) => {
                    return InputOutcome::Action(Action::CancelScheduledTask(task_id.clone()));
                }
                Some(TaskEntry::Workflow {
                    name, stoppable, ..
                }) => {
                    if *stoppable {
                        return InputOutcome::Action(Action::SendSlashCommandPreservingDraft(
                            format!("/workflow stop {name}"),
                        ));
                    }
                }
                Some(TaskEntry::Header { .. }) => {}
                None => {}
            }
        }
        if key!('y').matches(key)
            && self.tasks.list_state.input_mode().is_none()
            && let Some(task_id) = self.tasks.selected_task_id().map(|s| s.to_string())
            && let Some(task) = self.session.bg_tasks.get(&task_id)
            && !task.stdout.is_empty()
        {
            let text = task.stdout.clone();
            self.copy_to_clipboard(&text);
            return InputOutcome::Changed;
        }
        if key!(Tab).matches(key) && self.tasks.list_state.input_mode().is_none() {
            self.tasks.overlay.focused = false;
            return InputOutcome::Action(Action::FocusPrompt);
        }
        let has_input = self.tasks.list_state.input_mode().is_some();
        let action = handle_overlay_key(&mut self.tasks.overlay, key).or_else(|| {
            if !has_input {
                handle_overlay_nav_key(&mut self.tasks.overlay, key)
            } else {
                None
            }
        });
        if let Some(action) = action {
            self.tasks.on_state_change();
            if !self.tasks.overlay.visible || !self.tasks.overlay.focused {
                self.set_active_pane(AgentPane::Scrollback, false);
            }
            return overlay_action_to_outcome(action);
        }
        if self.tasks.handle_key(key) {
            InputOutcome::Changed
        } else {
            InputOutcome::Unchanged
        }
    }
    /// Subagent-pane-focused key handling.
    pub(super) fn handle_catalog_key(
        &mut self,
        key: &KeyEvent,
        _registry: &ActionRegistry,
    ) -> InputOutcome {
        use crate::views::overlay::{handle_overlay_key, handle_overlay_nav_key};
        let has_input = self.catalog.list_state.input_mode().is_some();
        let action = handle_overlay_key(&mut self.catalog.overlay, key).or_else(|| {
            if !has_input {
                handle_overlay_nav_key(&mut self.catalog.overlay, key)
            } else {
                None
            }
        });
        if let Some(action) = action {
            self.catalog.on_state_change();
            if !self.catalog.overlay.visible || !self.catalog.overlay.focused {
                self.set_active_pane(AgentPane::Scrollback, false);
            }
            return overlay_action_to_outcome(action);
        }
        if key.code == crossterm::event::KeyCode::Enter
            && key.modifiers == crossterm::event::KeyModifiers::NONE
        {
            if let Some((kind, name)) = self.catalog.selected_entry() {
                return InputOutcome::Action(Action::ViewCatalogEntry {
                    kind: kind.to_owned(),
                    name: name.to_owned(),
                });
            }
            return InputOutcome::Unchanged;
        }
        if self.catalog.handle_key(key) {
            InputOutcome::Changed
        } else {
            InputOutcome::Unchanged
        }
    }
    /// Handle a normalized scroll event at a screen position.
    /// Hit-tests against pane areas to decide what to scroll:
    /// Scrollback area: scroll the scrollback (uses accelerated line count)
    pub fn handle_scroll(&mut self, lines: i32, col: u16, row: u16) {
        if self.show_workflows {
            let runs = self.workflow_runs_newest_first();
            let mut view = self.workflows_view.clone();
            view.handle_scroll(lines, col, row, &runs);
            self.workflows_view = view;
            return;
        }
        if self.show_goal_detail {
            return;
        }
        if let Some(ref mut modal) = self.active_modal {
            use crate::views::modal::ActiveModal;
            match modal {
                ActiveModal::CommandPalette { state, .. }
                | ActiveModal::ArgPicker { state, .. }
                | ActiveModal::SessionPicker { state, .. }
                | ActiveModal::DocPicker { state, .. } => {
                    let delta = lines.unsigned_abs() as usize;
                    let current = state.scroll_offset.unwrap_or(0);
                    let new_offset = if lines > 0 {
                        current + delta
                    } else {
                        current.saturating_sub(delta)
                    };
                    state.scroll_offset = Some(new_offset);
                    state.hovered = None;
                    return;
                }
                ActiveModal::DocViewer { scroll, .. }
                | ActiveModal::RememberNoteReview { scroll, .. } => {
                    crate::views::modal::apply_doc_scroll_delta(scroll, lines);
                    return;
                }
                _ => {}
            }
        }
        if let Some(ref mut viewer) = self.block_viewer {
            viewer.handle_scroll(lines);
            return;
        }
        if self.rewind_state.is_some() {
            if let Some(ref mut rw) = self.rewind_state {
                crate::views::rewind::move_cursor(&mut rw.phase, lines.signum());
                self.sync_rewind_anchor_to_picker();
            }
            return;
        }
        self.dismiss_jump_picker_if_suppressed();
        if let Some(ref mut js) = self.jump_state {
            crate::views::jump::move_cursor(js, lines.signum());
            self.sync_jump_preview();
            return;
        }
        if let Some(ref mut viewer) = self.line_viewer {
            if let Some(area) = viewer.last_popup_area
                && (area.contains((col, row).into()) || viewer.list_state.scrollbar_hit(col, row))
            {
                viewer
                    .list_state
                    .handle_scroll_event(lines, col, row, &viewer.lines);
            }
            return;
        }
        if let Some(ref mut btw) = self.btw_state
            && matches!(btw, crate::views::btw_overlay::BtwOverlayState::Done { .. })
            && self.last_btw_area.area() > 0
            && self.last_btw_area.contains((col, row).into())
        {
            use crate::views::btw_overlay::DONE_MAX_BODY_LINES;
            let max_body = DONE_MAX_BODY_LINES as usize;
            let content_width = self.last_btw_area.width.saturating_sub(4) as usize;
            let max_off = btw.max_scroll_offset(content_width, max_body);
            if lines > 0 {
                btw.scroll_down(lines as usize, max_off);
            } else {
                btw.scroll_up((-lines) as usize);
            }
            return;
        }
        if let Some(hd_area) = self.history_dropdown_area
            && hd_area.contains((col, row).into())
            && self.prompt.history_search.is_active()
        {
            let moved = if lines > 0 {
                self.prompt.history_search.move_down()
            } else if lines < 0 {
                self.prompt.history_search.move_up()
            } else {
                false
            };
            if moved && self.prompt.history_search.is_browse() {
                self.populate_prompt_from_history_selection();
            }
            return;
        }
        if let Some(dd_area) = self.dropdown_items_area
            && dd_area.contains((col, row).into())
        {
            self.prompt
                .file_search
                .move_selection(lines.signum() as isize);
            return;
        }
        if let Some(dd_area) = self.slash_dropdown_items_area
            && dd_area.contains((col, row).into())
        {
            self.prompt.slash_scroll_selection(lines.signum() as isize);
            self.prompt.slash_preview_current_selection();
            return;
        }
        if let Some(dd_area) = self.completion_dropdown_items_area
            && dd_area.contains((col, row).into())
        {
            self.prompt
                .completion_dropdown_scroll(lines.signum() as isize);
            return;
        }
        if self.question_view.is_some() && self.pane_areas.prompt.contains((col, row).into()) {
            if self
                .inline_prompt_area
                .is_some_and(|r| r.contains((col, row).into()))
            {
                let kind = if lines > 0 {
                    MouseEventKind::ScrollDown
                } else {
                    MouseEventKind::ScrollUp
                };
                let event = MouseEvent {
                    kind,
                    column: col,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                };
                let _ = self.prompt.handle_mouse(&event);
            } else if let Some((scroll_top, scroll_bottom)) = self.question_scroll_region
                && row >= scroll_top
                && row < scroll_bottom
            {
                self.apply_question_scroll(lines);
            }
            return;
        }
        let target = self
            .pane_areas
            .hit_test(col, row)
            .unwrap_or(ActivePane::Scrollback);
        match target {
            ActivePane::Scrollback => {
                if lines > 0 {
                    self.scrollback.scroll_down(lines as u16);
                } else {
                    self.scrollback.scroll_up((-lines) as u16);
                }
            }
            ActivePane::Todo => {
                self.todo.handle_scroll(lines, col, row);
            }
            ActivePane::Queue => {
                self.queue.handle_scroll(lines, col, row);
            }
            ActivePane::Tasks => {
                self.tasks.handle_scroll(lines, col, row);
            }
            ActivePane::Catalog => {
                self.catalog.handle_scroll(lines, col, row);
            }
            ActivePane::Dock => match self.dock_section_at(self.pane_areas.dock, row) {
                Some(crate::views::dock::Section::Queued) | None => {
                    self.queue.handle_scroll(lines, col, row);
                }
                Some(section) => {
                    self.scroll_dock_section(section, lines as isize);
                    if self.active_pane == ActivePane::Dock {
                        let items = self.dock_items();
                        self.dock_cursor = self.dock_cursor.min(items.len().saturating_sub(1));
                    }
                }
            },
            ActivePane::Prompt => {
                if self.question_view.is_some() {
                    return;
                }
                let kind = if lines > 0 {
                    MouseEventKind::ScrollDown
                } else {
                    MouseEventKind::ScrollUp
                };
                let event = MouseEvent {
                    kind,
                    column: col,
                    row,
                    modifiers: KeyModifiers::NONE,
                };
                if !self.prompt.handle_mouse_scroll(&event) {
                    if lines > 0 {
                        self.scrollback.scroll_down(lines as u16);
                    } else {
                        self.scrollback.scroll_up((-lines) as u16);
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod scroll_granularity_tests {
    use super::super::test_fixtures::make_agent;
    use crate::views::prompt_widget::PromptStyle;
    use crate::views::suggestion_controller::{
        CompletionDropdownState, CompletionItemParsed, SuggestionSource,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    /// Selection dropdowns step exactly one item per wheel dispatch: a 3-line notch (or accelerated trackpad flush) must not skip items.
    #[test]
    fn wheel_notch_over_slash_dropdown_moves_selection_one_step() {
        let mut agent = make_agent();
        agent.prompt.set_text("/");
        agent.prompt.refresh_slash(&agent.session.models);
        assert!(agent.prompt.slash_open(), "precondition: dropdown open");
        assert!(
            agent.prompt.slash_snapshot().matches.len() >= 3,
            "precondition: enough builtin commands to skip over"
        );
        assert_eq!(agent.prompt.slash_snapshot().selected, 0);
        agent.slash_dropdown_items_area = Some(Rect::new(0, 0, 40, 8));
        agent.handle_scroll(3, 5, 4);
        assert_eq!(
            agent.prompt.slash_snapshot().selected,
            1,
            "3-line wheel notch must move the slash selection by exactly 1"
        );
        agent.handle_scroll(-3, 5, 4);
        assert_eq!(
            agent.prompt.slash_snapshot().selected,
            0,
            "-3-line wheel notch must move the slash selection by exactly -1"
        );
    }
    fn completion_item(label: &str) -> CompletionItemParsed {
        CompletionItemParsed {
            display: label.into(),
            description: String::new(),
            insert_text: label.into(),
            source: SuggestionSource::History,
            priority: 0,
            replace_range: None,
            token_text: None,
            truncated: false,
        }
    }
    #[test]
    fn wheel_notch_over_completion_dropdown_moves_selection_one_step() {
        let mut agent = make_agent();
        agent.prompt.suggestions.dropdown = CompletionDropdownState {
            open: true,
            items: vec![
                completion_item("a"),
                completion_item("b"),
                completion_item("c"),
            ],
            selected: 0,
            ..Default::default()
        };
        agent.completion_dropdown_items_area = Some(Rect::new(0, 0, 40, 8));
        agent.handle_scroll(3, 5, 4);
        assert_eq!(
            agent.prompt.suggestions.dropdown.selected, 1,
            "3-line wheel notch must move the completion selection by exactly 1"
        );
        agent.handle_scroll(-3, 5, 4);
        assert_eq!(
            agent.prompt.suggestions.dropdown.selected, 0,
            "-3-line wheel notch must move the completion selection by exactly -1"
        );
    }
    #[test]
    fn wheel_over_prompt_scrolls_conversation() {
        let mut agent = make_agent();
        agent.pane_areas.prompt = Rect::new(0, 10, 80, 4);
        let mut buf = Buffer::empty(agent.pane_areas.prompt);
        agent.prompt.draw(
            &mut buf,
            agent.pane_areas.prompt,
            None,
            &PromptStyle::default(),
            None,
            None,
        );
        for i in 0..30 {
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::agent_message(
                    format!("line {i}"),
                ));
        }
        agent.scrollback.prepare_layout(80, 10);
        agent.scrollback.goto_bottom();
        let before = agent.scrollback.scroll_info().0;
        assert!(before > 0, "setup: conversation has earlier content");
        agent.handle_scroll(-3, 5, 11);
        assert_eq!(agent.scrollback.scroll_info().0, before - 3);
    }
    #[test]
    fn wheel_over_scrollable_prompt_keeps_conversation_position() {
        let mut agent = make_agent();
        agent.pane_areas.prompt = Rect::new(0, 10, 20, 4);
        agent.prompt.set_text(
            &(0..20)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        agent.prompt.set_cursor(0);
        agent.prompt.set_scroll(0);
        let mut buf = Buffer::empty(agent.pane_areas.prompt);
        agent.prompt.draw(
            &mut buf,
            agent.pane_areas.prompt,
            None,
            &PromptStyle::default(),
            None,
            None,
        );
        for i in 0..30 {
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::agent_message(
                    format!("message {i}"),
                ));
        }
        agent.scrollback.prepare_layout(80, 10);
        agent.scrollback.goto_bottom();
        agent.scrollback.scroll_up(5);
        let conversation_before = agent.scrollback.scroll_info().0;
        agent.handle_scroll(3, 5, 11);
        assert_eq!(agent.scrollback.scroll_info().0, conversation_before);
    }
    #[test]
    fn wheel_over_fullscreen_overlays_never_scrolls_panes_beneath() {
        let mut agent = make_agent();
        agent.pane_areas.scrollback = Rect::new(0, 0, 80, 10);
        for i in 0..30 {
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::agent_message(
                    format!("line {i}"),
                ));
        }
        agent.scrollback.prepare_layout(80, 10);
        agent.scrollback.scroll_up(5);
        let before = agent.scrollback.scroll_info().0;
        assert!(before > 0, "setup: scrollback holds a real offset");
        agent.show_workflows = true;
        agent.handle_scroll(3, 5, 4);
        agent.handle_scroll(-3, 5, 4);
        assert_eq!(
            agent.scrollback.scroll_info().0,
            before,
            "wheel must not leak through the /workflow runs modal"
        );
        agent.show_workflows = false;
        agent.show_goal_detail = true;
        agent.handle_scroll(-3, 5, 4);
        assert_eq!(
            agent.scrollback.scroll_info().0,
            before,
            "wheel must not leak through the goal detail overlay"
        );
    }
}
#[cfg(test)]
mod mouse_reporting_registry_tests {
    use super::super::{AgentPane, test_fixtures::make_agent};
    use crate::actions::ActionRegistry;
    use crate::app::actions::Action;
    use crate::app::app_view::InputOutcome;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    fn ctrl_r() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
    }
    #[test]
    fn mouse_toggle_chord_follows_live_mode_registry() {
        for mode in [
            crate::app::ScreenMode::Fullscreen,
            crate::app::ScreenMode::Inline,
        ] {
            let mut agent = make_agent();
            agent.set_active_pane(AgentPane::Scrollback, true);
            let registry = ActionRegistry::defaults_with_config_for(mode, true);
            assert!(matches!(
                agent.handle_input(&ctrl_r(), &registry),
                InputOutcome::Action(Action::ToggleMouseCapture)
            ));
        }
        let mut agent = make_agent();
        agent.set_active_pane(AgentPane::Scrollback, true);
        let registry =
            ActionRegistry::defaults_with_config_for(crate::app::ScreenMode::Minimal, true);
        assert!(
            registry
                .find(crate::actions::ActionId::ToggleMouseCapture)
                .is_none()
        );
        assert!(!matches!(
            agent.handle_input(&ctrl_r(), &registry),
            InputOutcome::Action(Action::ToggleMouseCapture)
        ));
    }
}
#[cfg(test)]
mod paste_routing_tests {
    use super::super::{AgentPane, test_fixtures::make_agent};
    use crate::actions::ActionRegistry;
    use crate::app::app_view::InputOutcome;
    use crate::scrollback::ScrollbackSearchState;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    #[test]
    fn scrollback_search_paste_stays_scoped_and_browse_is_inert() {
        let mut agent = make_agent();
        agent.vim_mode = false;
        agent.set_active_pane(AgentPane::Scrollback, true);
        agent.prompt.set_text("hidden prompt");
        agent.scrollback_search = Some(ScrollbackSearchState::open());
        let registry = ActionRegistry::defaults();
        let _ = agent.handle_input(&Event::Paste("ab".to_owned()), &registry);
        let _ = agent.handle_input(
            &Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            &registry,
        );
        let outcome = agent.handle_input(&Event::Paste("中\r\n".to_owned()), &registry);
        assert!(matches!(outcome, InputOutcome::Changed));
        assert_eq!(
            agent
                .scrollback_search
                .as_ref()
                .map(ScrollbackSearchState::query),
            Some("a中b")
        );
        assert_eq!(agent.prompt.text(), "hidden prompt");
        agent.scrollback_search.as_mut().unwrap().accept();
        let outcome = agent.handle_input(&Event::Paste("ignored".to_owned()), &registry);
        assert!(matches!(outcome, InputOutcome::Unchanged));
        assert_eq!(
            agent
                .scrollback_search
                .as_ref()
                .map(ScrollbackSearchState::query),
            Some("a中b")
        );
        assert_eq!(agent.prompt.text(), "hidden prompt");
        agent.scrollback_search = None;
        let outcome = agent.handle_input(&Event::Paste("forwarded".to_owned()), &registry);
        assert!(matches!(
            outcome,
            InputOutcome::ActionThenForward(crate::app::actions::Action::FocusPrompt)
        ));
        assert_eq!(agent.prompt.text(), "hidden prompt");
    }
}
