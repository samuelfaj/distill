// Modified for Distill by Samuel Fajreldines, 2026.
use indexmap::IndexMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::animation::{Animation, NEEDS_INPUT_BLINK_DIVISOR, PaintedAnimations, SPINNER_DIVISOR};
pub use super::chrome::HeaderUpgradeCta;
use super::layout::{MIN_DASHBOARD_WIDTH, compute_layout};
use super::row::{DashboardRow, build_rows_with_roster, build_rows_with_workspace};
use super::state::{
    DashboardRowId, DashboardState, DashboardStopAction, Filter, Focusable, Grouping,
    LocationPickerState, RenameDraft, RowState, SectionKey,
};
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::render::line_utils::truncate_str;
use crate::theme::Theme;
use crate::util::format_time_ago;
use crate::views::dashboard::row_title::RowTitle;

// Row markers use the filled (◆) / hollow (◇) diamonds from `crate::glyphs` (with CP437 fallbacks on legacy consoles)
// The dashboard uses diamonds instead of circles so this view reads differently from sibling activity views, which use circles
// Filled marks the non-working states that need a strong visual presence (needs-input, completed, failed, blocked); hollow marks idle rows

fn ensure_peek_viewport_lifecycle(
    state: &mut DashboardState,
    agents: &mut IndexMap<AgentId, AgentView>,
) {
    if state.attached_agent.is_some() {
        return;
    }
    // When a peek row exists, begin or keep the viewport lease; otherwise restore the agent viewport
    let Some(row) = state.peek.as_ref().map(|p| p.row.clone()) else {
        state.restore_peek_viewport(agents);
        return;
    };
    if state
        .peek_viewport
        .as_ref()
        .is_some_and(|lease| lease.row == row)
    {
        return;
    }
    if super::state::scrollback_available_for_row(&row, agents) {
        state.begin_peek_viewport(row, agents);
    } else {
        state.restore_peek_viewport(agents);
    }
}

// The thin left bar marking the selected row is `crate::glyphs::selection_bar()`, with a `│` fallback on legacy CP437 consoles
// It is painted on every content line of a selected row so it spans the row's full visual height

/// Per-row visual height in cells: a title row, a secondary row, and a one-cell breathing gap.
const ROW_HEIGHT: u16 = 3;
/// Per-group-header visual height in cells: a label row and a one-cell breathing gap.
const GROUP_HEADER_HEIGHT: u16 = 2;

/// The user must press Space to peek (which routes the permission question and options into the
/// peek panel) and then a number key to answer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_dashboard(
    buf: &mut Buffer,
    area: Rect,
    state: &mut DashboardState,
    agents: &mut IndexMap<AgentId, AgentView>,
    registry: &crate::actions::ActionRegistry,
    // App-level double-press confirmation hint (e.g. "press again to quit" for Ctrl+Q / Ctrl+C / Ctrl+D).
    // Threaded to the footer so the session-less dashboard shows the same feedback the agent view does
    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
    // Leader-mode session roster (FleetView)
    // Empty in non-leader mode, so no roster-only rows are appended
    roster: &[crate::app::roster::RosterEntry],
    workspace_dashboard_enabled: bool,
    // Dashboard v2 membership, layout, and provisional live rows.
    row_inputs: super::row::WorkspaceRowInputs<'_>,
    dashboard_session_picker: Option<
        &mut crate::views::session_picker_surface::SessionPickerSurface,
    >,
    // Whether the local on-disk session roster is still being fetched (non-leader mode)
    // When true and there's nothing to show yet, the empty body reads "Loading sessions…" instead of the "no agents yet" hint
    // That way a fresh open doesn't flash an empty-looking screen
    dashboard_sessions_loading: bool,
    upgrade_cta: Option<HeaderUpgradeCta<'_>>,
    // App-level billing mirror the `/usage` modal renders its allowance from
    credit_balance: Option<&crate::views::credit_bar::CreditBalance>,
) -> Option<(u16, u16)> {
    state.workspace_membership_mode = workspace_dashboard_enabled;
    // Cache whether a pinned (non-dismissible) promo CTA is live so the key handler can steal Ctrl+O for it; the dispatch re-resolves the gate
    state.pinned_upgrade_cta_live = upgrade_cta.is_some_and(|cta| cta.pinned);
    state.clear_chrome_hit_areas();
    // Re-anchor selection BEFORE we build the rows so that the visible set drives selection clamping
    let theme = Theme::current();
    state.last_area = area;

    // Paint the full area with the theme's base background BEFORE any sub-renderer runs (mirrors `welcome::render` and `PromptWidget::draw`)
    // Cells no sub-renderer touches in a frame would keep the previous frame's paint, and the dashboard would look like it doesn't cover the panel
    // Blank rows between the last list row and the dispatch input, and trailing whitespace past short row content, are the usual cases
    buf.set_style(area, ratatui::style::Style::default().bg(theme.bg_base));

    let home = cached_home();
    let grouping = if workspace_dashboard_enabled {
        let grouping = row_inputs.grouping();
        state.observe_workspace_grouping(grouping);
        grouping
    } else {
        state.clear_workspace_grouping();
        state.grouping
    };
    let rows = if workspace_dashboard_enabled {
        build_rows_with_workspace(agents, row_inputs, &state.filter, home)
    } else {
        build_rows_with_roster(
            agents,
            &state.pinned,
            &state.reorder,
            state.grouping,
            &state.filter,
            home,
            roster,
        )
    };
    state.painted_animations = PaintedAnimations::default();
    // Chat-conversation roster rows can't be deleted from the dashboard yet; record them so the `[✗]` button and the Ctrl+X arm both skip them
    state.conversation_row_ids = if workspace_dashboard_enabled {
        Default::default()
    } else {
        roster
            .iter()
            .filter(|e| e.origin.kind == "conversation")
            .map(|e| e.session_id.clone())
            .collect()
    };
    state.reanchor_selection(&rows);

    // DO NOT GC pinned/reorder at render time. A GC here sees only the post-filter row list, so a
    // user-typed filter that hid a pinned row would drop the pin from the in-memory set.

    // In popup mode we paint ONLY a compact dashboard banner at the top (rows in a bordered panel),
    // and the popup fills the rest.
    if state.attached_agent.is_some() {
        state.peek_close_rect = None;
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        state.file_search_dropdown_items_area = None;
        state.dispatch_rect = None;
        let popup = popup_rect(area);
        let banner_h = popup.y.saturating_sub(area.y);
        let banner_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: banner_h,
        };
        render_dashboard_banner(buf, banner_area, &theme, &rows, grouping, state);
        return None;
    }

    // Peek: list-first allocation (see layout::allocate_peek and docs/internal/33-dashboard-peek-responsive-layout.md)
    // The provisional layout gives the dispatch width for reply wrapping before we decide whether peek fits
    let mut layout = compute_layout(area, false);
    let fixed = super::layout::chrome_overhead(area);
    let reply_text_w = layout.dispatch.width.saturating_sub(6);

    if !state.search_mode {
        match state.selected.clone() {
            Some(sel) => match super::peek::compute_peek_fields(&sel, agents) {
                Some(fields) => {
                    let question = fields.question.is_some();
                    let peek_min = if question {
                        super::layout::PEEK_MIN_BOX_QUESTION
                    } else {
                        super::layout::PEEK_MIN_BOX_LIVE_TAIL
                    };
                    let content_rows = if question {
                        1 + fields.options.len().min(9) as u16
                    } else {
                        let reply_rows = super::peek::reply_row_count(
                            &state.peek_reply,
                            reply_text_w,
                            super::peek::MAX_REPLY_ROWS,
                        );
                        let max_content = super::layout::max_peek_content_rows(area);
                        // The middle content width is the dispatch box width minus borders and insets
                        let middle_w = layout.dispatch.width.saturating_sub(4);
                        let (body_measured, pin_user) =
                            super::state::scrollback_mut_for_row(&sel, agents)
                                .map(|sb| {
                                    (
                                        super::peek_tail::densified_body_line_count(sb, middle_w),
                                        super::peek_tail::scrollback_has_last_user(sb),
                                    )
                                })
                                .unwrap_or((0, false));
                        super::layout::peek_live_tail_desired_content(
                            max_content,
                            reply_rows,
                            body_measured,
                            pin_user,
                        )
                        .content_rows
                    };
                    let alloc =
                        super::layout::allocate_peek(area.height, fixed, content_rows, peek_min);
                    if alloc.show_peek {
                        state.set_peek_reply_target_cwd(peeked_agent_cwd(&sel, agents));
                        let badge = super::peek::peek_model_and_mode(&sel, agents);
                        match state.peek.as_mut() {
                            Some(p) => {
                                if p.apply_fields(sel, fields) {
                                    state.clear_peek_reply();
                                }
                            }
                            None => {
                                state.set_peek(Some(super::peek::PeekPanelState::new(sel, fields)))
                            }
                        }
                        if let Some(p) = state.peek.as_mut() {
                            p.model_name = badge.model;
                            p.auto_approve = badge.yolo;
                            p.auto = badge.auto;
                            p.mode_label = badge.mode_label;
                        }
                        layout =
                            super::layout::compute_layout_with_peek_box(area, alloc.peek_box_h);
                    } else {
                        state.set_peek_reply_target_cwd(None);
                        state.set_peek(None);
                    }
                }
                None => {
                    state.set_peek_reply_target_cwd(None);
                    state.set_peek(None);
                }
            },
            None => {
                state.set_peek_reply_target_cwd(None);
                state.set_peek(None);
            }
        }
    }

    ensure_peek_viewport_lifecycle(state, agents);

    if state.peek.is_none() && area.height > 8 && !state.dispatch.text().is_empty() {
        let rows = dispatch_text_rows(state, layout.dispatch.width, area.height);
        if rows > 1 {
            layout = super::layout::compute_layout_with_dispatch(area, false, rows);
        }
    }

    super::chrome::render_header(
        buf,
        layout.header,
        &theme,
        &rows,
        state,
        registry,
        upgrade_cta,
    );
    super::chrome::render_actions_row(
        buf,
        layout.actions,
        &theme,
        state,
        registry,
        workspace_dashboard_enabled,
    );

    // Body: key off visible rows (local agents and roster), not the local map alone
    if rows.is_empty() {
        if state.filter.is_active() {
            render_no_match(buf, layout.list, &theme, &state.filter);
        } else {
            render_empty_state(buf, layout.list, &theme, dashboard_sessions_loading);
        }
    } else if area.width < MIN_DASHBOARD_WIDTH {
        render_narrow_rows_with_grouping(buf, layout.list, &theme, &rows, grouping, state);
    } else {
        render_rows_with_grouping(buf, layout.list, &theme, &rows, grouping, state);
    }

    // Compute the contextual placeholder hint and footer mode once here so both sub-renderers stay pure functions of the selected-row state
    let selected_state = state
        .selected
        .as_ref()
        .and_then(|sel| rows.iter().find(|r| r.id == *sel).map(|r| r.state));
    state.selected_stop_action = workspace_dashboard_enabled
        .then(|| match state.selected.as_ref() {
            Some(DashboardRowId::TopLevel(id)) => agents
                .get(id)
                .map(|agent| crate::app::dashboard_stop_readiness(agent).action()),
            Some(DashboardRowId::Workspace { .. }) => Some(DashboardStopAction::Archive),
            Some(DashboardRowId::Subagent { .. } | DashboardRowId::Roster { .. }) | None => None,
        })
        .flatten();
    let peek_active = state.peek_owns_input();

    // Peek REPLACES the dispatch input when active (a single rounded box at the same screen position, instead of a separate panel floating above it)
    // When peek is closed, the dispatch input renders normally
    let dispatch_cursor = if peek_active {
        // Peek has no in-box close button
        // `render_peek_panel` returns the `❯ reply` caret position so the terminal cursor parks in the live reply input
        // It also returns the reply row's rect, recorded for click-to-focus and drag-selection mouse routing
        state.peek_close_rect = None;
        // The peek box replaces the dispatch box, which would otherwise own the overlay.
        let voice_listening = state.voice_listening;
        let voice_interim = state.voice_interim.clone();
        let multiline = state.multiline_mode;
        let peeked_row = state.peek.as_ref().map(|p| p.row.clone());
        let question_pending = state.peek.as_ref().is_some_and(|p| p.question.is_some());
        let (empty_hint, has_scrollback) = match peeked_row.as_ref() {
            Some(DashboardRowId::Subagent {
                parent,
                child_session_id,
            }) => {
                let parent_ok = agents
                    .get(parent)
                    .is_some_and(|p| p.subagent_sessions.contains_key(child_session_id));
                let loaded = agents
                    .get(parent)
                    .is_some_and(|p| p.has_subagent_view(child_session_id));
                if parent_ok && !loaded {
                    (Some("Subagent not loaded"), false)
                } else {
                    (None, loaded)
                }
            }
            Some(row) => (
                None,
                super::state::scrollback_available_for_row(row, agents),
            ),
            None => (None, false),
        };
        let render = if let Some(panel) = state.peek.as_ref() {
            let live_tail = if !question_pending && has_scrollback {
                peeked_row
                    .as_ref()
                    .and_then(|row| super::state::scrollback_mut_for_row(row, agents))
                    .map(|scrollback| super::peek::PeekLiveTailArgs { scrollback })
            } else {
                None
            };
            super::peek::render_peek_panel(
                buf,
                layout.dispatch,
                panel,
                &mut state.peek_reply,
                &theme,
                voice_listening,
                voice_interim.as_deref(),
                multiline,
                Some(layout.list).filter(|r| r.area() > 0),
                live_tail,
                empty_hint,
            )
        } else {
            Default::default()
        };
        // Peek auto-opens for the selected row, so a toast from a row action would otherwise never be painted.
        paint_dispatch_feedback_badge(buf, layout.dispatch, &theme, state.error_toast.as_deref());
        state.peek_reply_rect = render.reply_rect;
        let cursor = render.caret;
        // The reply is a full PromptWidget, so its `@` file-context picker paints ABOVE the peek box (same chrome as the dispatch box's)
        // Slash completion stays inert for the reply
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        if state.peek_reply.file_search_visible() {
            state.file_search_dropdown_items_area = render_file_search_dropdown_for(
                buf,
                area,
                layout.dispatch,
                &theme,
                &mut state.peek_reply.file_search,
            );
        } else {
            state.file_search_dropdown_items_area = None;
        }
        // Peek replaces the input box, so there is no input to click-to-focus
        state.dispatch_rect = None;
        cursor
    } else {
        state.peek_close_rect = None;
        state.peek_reply_rect = None;
        // Record the box rect so a click anywhere on it focuses the input (see `handle_mouse`)
        state.dispatch_rect = Some(layout.dispatch);
        let cursor = render_dispatch(
            buf,
            layout.dispatch,
            &theme,
            state,
            Some(layout.list).filter(|r| r.area() > 0),
        );
        // Completion dropdowns paint ABOVE the dispatch box
        // The `@` file-search picker and the `/` slash dropdown never render together
        // File search wins while the user is mid-`@token`; otherwise the slash dropdown shows
        if state.dispatch.file_search_visible() {
            render_file_search_dropdown(buf, area, layout.dispatch, &theme, state);
            state.slash_dropdown_items_area = None;
            state.slash_dropdown_hit = Default::default();
        } else {
            render_slash_dropdown(buf, area, layout.dispatch, &theme, state);
            state.file_search_dropdown_items_area = None;
        }
        cursor
    };

    // Footer.
    render_footer(
        buf,
        layout.footer,
        &theme,
        state,
        registry,
        selected_state,
        peek_active,
        pending_hint,
    );

    if let Some(surface) = dashboard_session_picker {
        if surface.loading {
            state.painted_animations.mark(Animation::Spinner);
        }
        let hit_areas = crate::views::session_picker_surface::render_session_picker(
            area,
            buf,
            &theme,
            crate::views::session_picker_surface::SessionPickerRenderMode::Modal {
                window: &mut surface.window,
                title: crate::views::session_picker_surface::DASHBOARD_PICKER_TITLE,
            },
            &mut crate::views::session_picker_surface::SessionPickerRenderCtx {
                state: &mut surface.state,
                sessions: surface.entries.as_deref(),
                cwd: &state.cwd,
                loading: surface.loading,
                pending_hint: None,
                shortcuts_area: None,
                content_results: None,
                content_loading: false,
                entries_query: surface.entries_query.as_deref(),
                tick: state.spinner_tick,
                grouped: true,
                source_filter: surface.source_filter,
                pending_delete: false,
                chat_mode: false,
            },
        );
        surface.state.hit_areas = (hit_areas.search_bar.width > 0).then_some(hit_areas);
        return None;
    }

    // Cheatsheet modal paints LAST so it overlays everything: the row list, the dispatch widget, the
    // footer hints. When Some, we suppress the dispatch cursor because input is routed to the modal
    // until it closes.
    if let Some(modal) = state.shortcuts_modal.as_mut() {
        crate::views::shortcuts_help::render_modal(
            buf,
            area,
            &modal.entries,
            &mut modal.state,
            &mut modal.window,
            modal.filter_active,
            &modal.collapsed_sections,
            &modal.expanded_ids,
            &modal.mode,
            &theme,
            /* compact */ false,
        );
        return None;
    }

    if let Some(modal) = state.usage_modal.as_mut() {
        crate::views::usage_modal::render_usage_modal(
            buf,
            area,
            modal,
            credit_balance,
            /* compact */ false,
            &theme,
        );
        return None;
    }

    // The location picker overlays everything too (mutually exclusive with the shortcuts modal in practice)
    // When open, input is routed to it, so the dispatch cursor is suppressed
    if let Some(modal) = state.location_picker.as_mut() {
        render_location_picker(buf, area, &theme, modal);
        return None;
    }

    // The worktree-label dialog overlays the dashboard while the user names the worktree for a dashboard-dispatched agent
    // Input is routed to it, so the dispatch cursor is suppressed
    if let Some(dialog) = state.worktree_dialog.as_ref() {
        crate::views::new_worktree_dialog::render_new_worktree_dialog(area, buf, dialog);
        return None;
    }

    // An active rename replaces the dispatch caret with its row-local editor caret.
    if let Some(pos) = rename_cursor_pos(state, &rows) {
        return Some(pos);
    }
    dispatch_cursor
}

const RENAME_PREFIX: &str = "rename: ";

fn rename_editor_view(draft: &RenameDraft, width: u16) -> (&str, u16) {
    let prefix_width = UnicodeWidthStr::width(RENAME_PREFIX) as u16;
    let editor_width = width.saturating_sub(prefix_width);
    let viewport = draft.viewport(editor_width as usize);
    let visible = draft
        .text()
        .get(viewport.visible_byte_range.clone())
        .unwrap_or("");
    let cursor_offset = prefix_width
        .saturating_add(viewport.cursor_display_column as u16)
        .min(width.saturating_sub(1));
    (visible, cursor_offset)
}

fn render_rename_editor(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    style: Style,
    draft: &RenameDraft,
) {
    if width == 0 {
        return;
    }
    let prefix_width = UnicodeWidthStr::width(RENAME_PREFIX) as u16;
    buf.set_span(
        x,
        y,
        &Span::styled(RENAME_PREFIX, style),
        prefix_width.min(width),
    );
    let (visible, _) = rename_editor_view(draft, width);
    if !visible.is_empty() && prefix_width < width {
        buf.set_span(
            x + prefix_width,
            y,
            &Span::styled(visible, style),
            width - prefix_width,
        );
    }
}

/// Return the active rename's caret when its row is visible.
fn rename_cursor_pos(state: &DashboardState, rows: &[DashboardRow]) -> Option<(u16, u16)> {
    let rn = state.rename.as_ref()?;
    let (_, rect) = state.row_rects.iter().find(|(id, _)| *id == rn.row)?;
    let row = rows.iter().find(|r| r.id == rn.row);
    let (marker_width, indent_width, icon_width) = row
        .map(|r| {
            (
                UnicodeWidthStr::width(crate::glyphs::selection_bar()) as u16,
                (r.indent as u16) * 2,
                UnicodeWidthStr::width(state_icon(r.state, state.spinner_tick)) as u16,
            )
        })
        .unwrap_or((1, 0, 1));
    let chrome_width = marker_width + 1 + indent_width + icon_width + 1;
    let content_x = rect.x.saturating_add(chrome_width);
    let content_width = rect.x.saturating_add(rect.width).saturating_sub(content_x);
    let (_, cursor_offset) = rename_editor_view(rn, content_width);
    let cursor_x = content_x
        .saturating_add(cursor_offset)
        .min(rect.x.saturating_add(rect.width.saturating_sub(1)));
    // Mirror `render_row`'s vertical centering so the caret lands on the title line (narrow-mode single-line rects yield offset 0)
    let title_y = rect.y + row.map_or(0, |r| row_content_offset(rect.height, r));
    Some((cursor_x, title_y))
}

/// The agent's prompt is then the only input bar on screen.
fn render_dashboard_banner(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    grouping: Grouping,
    state: &mut DashboardState,
) {
    use ratatui::widgets::{Block, Borders, Widget};

    state.row_rects.clear();
    state.row_delete_rects.clear();
    state.section_rects.clear();
    if area.area() == 0 || area.height < 3 {
        return;
    }

    // Build the title chip: `Dashboard · N agents · M working`.
    let mut total = 0usize;
    let mut working = 0usize;
    let mut needs_input = 0usize;
    for r in rows.iter().filter(|r| r.indent == 0) {
        total += 1;
        if r.state == RowState::Working {
            working += 1;
        }
        if r.state == RowState::NeedsInput {
            needs_input += 1;
        }
    }
    let agent_word = if total == 1 { "agent" } else { "agents" };
    let mut title_parts: Vec<String> = vec!["Dashboard".to_string()];
    title_parts.push(format!("{total} {agent_word}"));
    if working > 0 {
        title_parts.push(format!("{working} working"));
    }
    if needs_input > 0 {
        title_parts.push(format!("{needs_input} awaiting"));
    }
    let title = format!(" {} ", title_parts.join(" · "));

    // Draw the bordered frame with the title centred on the top edge.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(theme.selection_border)
                .bg(theme.bg_base),
        )
        .title(
            Line::from(title).style(
                Style::default()
                    .fg(theme.text_primary)
                    .bg(theme.bg_base)
                    .add_modifier(Modifier::BOLD),
            ),
        );
    let inner = block.inner(area);
    block.render(area, buf);

    // Render rows inside the bordered area. Clip to the inner height so we never overrun the border.
    if inner.area() == 0 {
        return;
    }
    if rows.is_empty() {
        let hint = " No sessions yet. Esc to dispatch one. ";
        let trunc = truncate_str(hint, inner.width as usize);
        buf.set_string(inner.x, inner.y, trunc, theme.dim().bg(theme.bg_base));
        return;
    }

    if inner.width < MIN_DASHBOARD_WIDTH {
        render_narrow_rows_with_grouping(buf, inner, theme, rows, grouping, state);
    } else {
        render_rows_with_grouping(buf, inner, theme, rows, grouping, state);
    }
}

/// Render the location picker modal over the dashboard.
/// Content-row hit areas from the render are stashed on the modal for the mouse handler.
fn render_location_picker(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    modal: &mut LocationPickerState,
) {
    use crate::views::modal_window::{
        ModalSizing, ModalWindowConfig, Shortcut, push_vim_nav_search_hint, render_modal_window,
    };
    use crate::views::picker::{
        PickerEntry, PickerRow, render_divider, render_picker_content,
        render_picker_search_bar_with_label,
    };

    let mut shortcuts = vec![
        Shortcut {
            label: "\u{2191}\u{2193} nav",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "Tab complete",
            clickable: false,
            id: 1,
        },
        Shortcut {
            label: "Enter select",
            clickable: false,
            id: 2,
        },
        Shortcut {
            label: "Esc close",
            clickable: false,
            id: 3,
        },
    ];
    // Show `i search` in the footer when vim nav mode is active (the picker starts in input mode, but Esc drops to nav under vim)
    push_vim_nav_search_hint(&mut shortcuts, modal.picker.search_active);
    let config = ModalWindowConfig {
        title: "Change directory",
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing::medium(),
        fold_info: None,
    };
    let Some(content) = render_modal_window(buf, area, &mut modal.window, &config, theme) else {
        modal.content_hits = None;
        return;
    };

    let mut content_area = content.content;

    // The effective candidate list, computed once and reused for both the worktree-eligibility check and the rows below
    let visible = modal.visible_candidates();
    // Worktrees require a git repo
    // The directory a selection would land in (the highlighted row, else the base cwd) decides whether the worktree toggle is meaningful
    // In a non-repo it's hidden and dispatch proceeds normally
    let target_dir = visible
        .get(modal.picker.selected)
        .map(|c| c.path.clone())
        .unwrap_or_else(|| modal.base_cwd.clone());
    let show_worktree = modal.target_is_repo(&target_dir);

    // Path input line: a visible, editable field (cursor always shown) so the user can see and type an absolute, `~`, or relative path
    // It doubles as the live filter for the candidate list below
    modal.worktree_hit.set(None);
    if content_area.height >= 2 {
        // Reserve room at the right of the path row for the worktree toggle button, only when the modal is wide enough to keep a usable path field
        // Otherwise the field spans the full width and the button is hidden
        let wt_text = if modal.worktree_mode {
            "[worktree:on]"
        } else {
            "[worktree:off]"
        };
        let wt_w = wt_text.len() as u16; // ASCII, so byte length equals display width
        const WT_GAP: u16 = 1;
        const MIN_PATH_W: u16 = 16;
        let (path_w, wt_rect) = if show_worktree && content_area.width >= wt_w + WT_GAP + MIN_PATH_W
        {
            (
                content_area.width - wt_w - WT_GAP,
                Some(Rect {
                    x: content_area.x + content_area.width - wt_w,
                    y: content_area.y,
                    width: wt_w,
                    height: 1,
                }),
            )
        } else {
            (content_area.width, None)
        };
        render_picker_search_bar_with_label(
            buf,
            content_area.x,
            content_area.y,
            path_w,
            theme,
            " path: ",
            &modal.picker,
            /* active */ false,
            /* show_hint */ false,
            Some(theme.bg_base),
        );
        modal.worktree_hit.set(wt_rect);
        if let Some(r) = wt_rect {
            // Dim label by default; brighten the text on hover, like other clickable buttons
            // When the toggle is on, the "on" word is green in either state so the active state reads at a glance
            let label_fg = if modal.worktree_hit.hovered {
                theme.text_primary
            } else {
                theme.gray
            };
            buf.set_string(
                r.x,
                r.y,
                wt_text,
                Style::default().fg(label_fg).bg(theme.bg_base),
            );
            if modal.worktree_mode
                && let Some(on_at) = wt_text.find("on")
            {
                buf.set_string(
                    r.x + on_at as u16,
                    r.y,
                    "on",
                    Style::default().fg(theme.accent_success).bg(theme.bg_base),
                );
            }
        }
        // Second header row: the inline error (red) when present, else a divider separating the input from the list
        if let Some(err) = modal.error.as_deref() {
            let err_line = truncate_str(err, content_area.width as usize);
            buf.set_string(
                content_area.x,
                content_area.y + 1,
                &err_line,
                Style::default().fg(theme.accent_error).bg(theme.bg_base),
            );
        } else {
            render_divider(
                buf,
                content_area.x,
                content_area.y + 1,
                content_area.width,
                theme,
                Some(theme.bg_base),
            );
        }
        content_area.y += 2;
        content_area.height = content_area.height.saturating_sub(2);
    }

    // Build entries from the effective list (`visible`, computed above).
    let badges: Vec<String> = visible
        .iter()
        .map(|c| match &c.worktree {
            Some(name) if name == &c.label => "worktree".to_string(),
            Some(name) => format!("worktree: {name}"),
            None => String::new(),
        })
        .collect();
    // Truncation priority: the directory name (label) is shown in full whenever it fits; the path
    // (right label) is truncated first.
    let details: Vec<String> = {
        const PREFIX: u16 = 2;
        const GAP: u16 = 2;
        const TRAILING: u16 = 1;
        let row_w = content_area.width.saturating_sub(1);
        visible
            .iter()
            .zip(&badges)
            .map(|(c, badge)| {
                let badge_w = if badge.is_empty() {
                    0
                } else {
                    badge.width() as u16 + 1
                };
                let reserved = PREFIX + c.label.width() as u16 + badge_w + GAP + TRAILING;
                let budget = row_w.saturating_sub(reserved) as usize;
                truncate_str(&c.detail, budget)
            })
            .collect()
    };
    let entries: Vec<PickerEntry<'_>> = visible
        .iter()
        .zip(&badges)
        .zip(&details)
        .enumerate()
        .map(|(vis, ((c, badge), detail))| {
            PickerEntry::Row(PickerRow {
                label: c.label.as_str(),
                right_label: detail.as_str(),
                selected: vis == modal.picker.selected,
                expanded: false,
                fields: &[],
                description_lines: &[],
                summary_lines: &[],
                dimmed: false,
                indent: 0,
                badge: badge.as_str(),
                badge_color: (!badge.is_empty()).then_some(theme.accent_user),
                collapsible: false,
                underline_last_desc: false,
            })
        })
        .collect();

    let hits = render_picker_content(
        buf,
        content_area,
        theme,
        &mut modal.picker,
        &entries,
        /* non_selectable */ &[],
        /* non_selectable_clickable */ &[],
        Some(theme.bg_base),
        /* loading */ false,
    );
    modal.content_hits = Some(hits);
}

/// One line in the dashboard's vertical stack: either a state-group header or a content row. The
/// per-row dot and state colour alone don't show at a glance how many sessions are awaiting input,
/// working, idle, or done. Subagent rows inherit their parent's group and never trigger a header.
enum DashboardLine<'a> {
    /// Cross-cutting "Pinned" section header (with count), emitted above the pinned block when grouping is ON.
    PinnedHeader {
        count: usize,
    },
    /// A textless horizontal rule, used when grouping is OFF to separate the pinned block from the rest without a labelled header.
    Divider,
    Header {
        state: RowState,
        count: usize,
    },
    Row(&'a DashboardRow),
    /// The Idle group's "N more" overflow toggle row, emitted at the bottom of a capped Idle group.
    /// `hidden` is the number of folded agents.
    /// `expanded` reflects [`super::state::DashboardState::idle_show_all`] so the label can flip to "show fewer".
    IdleOverflow {
        hidden: usize,
        expanded: bool,
    },
}

/// Walk `rows` and intersperse `DashboardLine::Header` entries at every top-level state transition.
/// `filter == Filter::State(_)`: the filtered view already contains only a single state, so the
/// header is redundant chrome.
fn build_dashboard_lines<'a>(
    rows: &'a [DashboardRow],
    grouping: Grouping,
    filter: &Filter,
    collapsed: &std::collections::HashSet<SectionKey>,
    idle_show_all: bool,
    search_active: bool,
) -> Vec<DashboardLine<'a>> {
    let groups_on = matches!(grouping, Grouping::State);
    let emit_state_headers = groups_on && !matches!(filter, Filter::State(_));

    // Pinned top-level agents are sorted to the front (see `sort_rows`), so they form a contiguous prefix of clusters
    // Split that prefix off as a dedicated "Pinned" section above the state / directory groups
    // That way a pinned (say) idle agent reads as pinned rather than landing under an "Idle" header
    let mut pinned_end = 0usize;
    let mut pinned_count = 0usize;
    {
        let mut i = 0usize;
        while i < rows.len() && rows.get(i).is_some_and(|r| r.indent == 0 && r.pinned) {
            pinned_count += 1;
            i += 1;
            // Glue the pinned parent's subagents into the section.
            while i < rows.len() && rows.get(i).is_some_and(|r| r.indent != 0) {
                i += 1;
            }
            pinned_end = i;
        }
    }

    let mut out: Vec<DashboardLine<'a>> = Vec::with_capacity(rows.len() + 6);
    if pinned_count > 0 {
        // Grouping ON gets a labelled "Pinned N" header above the block
        // Grouping OFF (Ctrl+G) gets no header; a textless divider separates the pinned block from the rest (only when there's a rest to separate)
        if groups_on {
            out.push(DashboardLine::PinnedHeader {
                count: pinned_count,
            });
        }
        // A collapsed "Pinned" section keeps its header but hides the pinned rows
        // Collapse only applies when grouping is ON (the header is the toggle; the grouping-OFF divider has none)
        let pinned_collapsed = groups_on && collapsed.contains(&SectionKey::Pinned);
        if !pinned_collapsed && let Some(pinned) = rows.get(..pinned_end) {
            out.extend(pinned.iter().map(DashboardLine::Row));
        }
        if !groups_on && pinned_end < rows.len() {
            out.push(DashboardLine::Divider);
        }
    }

    let rest = rows.get(pinned_end..).unwrap_or(&[]);
    if !emit_state_headers {
        out.extend(rest.iter().map(DashboardLine::Row));
        return out;
    }
    let mut last_top_state: Option<RowState> = None;
    // Whether the section currently being emitted is collapsed; when so its rows (and their subagents) are skipped but the header stays
    let mut current_collapsed = false;
    // Idle-overflow cap bookkeeping for the group currently being emitted. Without the `search_active`
    // check, an empty search query would leave old idle agents folded.
    let idle_cap_active = matches!(filter, Filter::None) && !search_active;
    let now = std::time::SystemTime::now();
    let mut idle_limit: Option<usize> = None;
    let mut idle_top_seen = 0usize;
    let mut idle_capping = false;
    let mut pending_overflow: Option<(usize, bool)> = None;
    for (i, row) in rest.iter().enumerate() {
        if row.indent == 0 && Some(row.state) != last_top_state {
            // Emit the overflow row of the group we're leaving before the new header, so it lands at the bottom of the Idle group
            if let Some((hidden, expanded)) = pending_overflow.take() {
                out.push(DashboardLine::IdleOverflow { hidden, expanded });
            }
            // Subagents are skipped over rather than breaking the count, since they share their parent's
            // group. The count reflects the true group size even when collapsed or capped `recent` tracks how
            // many are inside the freshness window (Idle only).
            let mut count = 0usize;
            let mut recent = 0usize;
            for r in rest.iter().skip(i) {
                if r.indent != 0 {
                    continue;
                }
                if r.state == row.state {
                    count += 1;
                    if idle_row_is_recent(r, now) {
                        recent += 1;
                    }
                } else {
                    break;
                }
            }
            out.push(DashboardLine::Header {
                state: row.state,
                count,
            });
            last_top_state = Some(row.state);
            current_collapsed = collapsed.contains(&SectionKey::State(row.state));
            // Reset or set the Idle cap for the new group
            idle_limit = None;
            idle_top_seen = 0;
            idle_capping = false;
            if row.state == RowState::Idle && idle_cap_active && !current_collapsed {
                // Keep the freshest agents: at least MAX_VISIBLE_IDLE, extended to cover everything still inside the freshness window
                // Only fold when it hides at least MIN_IDLE_FOLD rows (a single folded row saves no space)
                let base_limit = MAX_VISIBLE_IDLE.max(recent).min(count);
                let base_hidden = count - base_limit;
                if base_hidden >= MIN_IDLE_FOLD {
                    idle_limit = Some(if idle_show_all { count } else { base_limit });
                    pending_overflow = Some((base_hidden, idle_show_all));
                }
            }
        }
        if current_collapsed {
            continue;
        }
        // Idle cap: once past the limit, skip over-cap top-level rows and their subagents (idle_capping latches until the next group)
        if let Some(limit) = idle_limit {
            if row.indent == 0 {
                idle_top_seen += 1;
                idle_capping = idle_top_seen > limit;
            }
            if idle_capping {
                continue;
            }
        }
        out.push(DashboardLine::Row(row));
    }
    if let Some((hidden, expanded)) = pending_overflow.take() {
        out.push(DashboardLine::IdleOverflow { hidden, expanded });
    }
    out
}

/// Maximum number of top-level Idle agents shown before the rest fold into the "N more" overflow row.
/// The Idle group is sorted most-recent-first, so the folded tail is always the oldest.
pub const MAX_VISIBLE_IDLE: usize = 8;

/// Idle agents last active within this window are never folded, even beyond [`MAX_VISIBLE_IDLE`].
/// A burst of fresh sessions stays visible; the count cap only hides genuinely *old* idle agents.
const IDLE_FRESHNESS: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Don't fold fewer than this many rows: a "1 older" overflow row costs the same vertical space as the single row it would hide.
const MIN_IDLE_FOLD: usize = 2;

/// Whether an Idle `row` was last active within [`IDLE_FRESHNESS`] of `now`.
/// Future timestamps (clock skew across pager processes / roster) count as recent.
fn idle_row_is_recent(row: &DashboardRow, now: std::time::SystemTime) -> bool {
    now.duration_since(row.last_change_at)
        .map(|age| age < IDLE_FRESHNESS)
        .unwrap_or(true)
}

/// Display-order list of keyboard cursor targets: section headers and visible rows. The list is
/// derived from the same [`build_dashboard_lines`] the renderer paints, so navigation and rendering
/// never disagree on what's on screen.
pub(crate) fn focusables(
    rows: &[DashboardRow],
    grouping: Grouping,
    filter: &Filter,
    collapsed: &std::collections::HashSet<SectionKey>,
    idle_show_all: bool,
    search_active: bool,
) -> Vec<Focusable> {
    build_dashboard_lines(
        rows,
        grouping,
        filter,
        collapsed,
        idle_show_all,
        search_active,
    )
    .into_iter()
    .filter_map(|line| match line {
        DashboardLine::PinnedHeader { .. } => Some(Focusable::Section(SectionKey::Pinned)),
        DashboardLine::Header { state, .. } => Some(Focusable::Section(SectionKey::State(state))),
        DashboardLine::Row(row) if !row.is_more_placeholder => Some(Focusable::Row(row.id.clone())),
        DashboardLine::IdleOverflow { .. } => Some(Focusable::IdleOverflow),
        _ => None,
    })
    .collect()
}

/// The section header that owns `row_id` under the current grouping and filter. `None` when no
/// headers are emitted (directory grouping, `s:state` filter) or the row isn't present in `rows` at
/// all. A row hidden inside a collapsed section therefore still resolves to its owning header.
pub(crate) fn section_of_row(
    rows: &[DashboardRow],
    grouping: Grouping,
    filter: &Filter,
    row_id: &super::DashboardRowId,
) -> Option<SectionKey> {
    let none_collapsed = std::collections::HashSet::new();
    let mut current: Option<SectionKey> = None;
    // `idle_show_all = true` disables the Idle cap here, mirroring the empty collapsed set
    // A row hidden by the cap must still resolve to its Idle header so `reanchor_selection` can move a stranded cursor
    for line in build_dashboard_lines(rows, grouping, filter, &none_collapsed, true, false) {
        match line {
            DashboardLine::PinnedHeader { .. } => current = Some(SectionKey::Pinned),
            DashboardLine::Header { state, .. } => current = Some(SectionKey::State(state)),
            DashboardLine::Row(row) if row.id == *row_id => return current,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
fn render_rows(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
) {
    render_rows_with_grouping(buf, area, theme, rows, state.grouping, state);
}

fn render_rows_with_grouping(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    grouping: Grouping,
    state: &mut DashboardState,
) {
    state.row_rects.clear();
    state.row_delete_rects.clear();
    state.section_rects.clear();
    state.idle_overflow_rect = None;
    if area.area() == 0 {
        return;
    }

    // Rows are 3 visual cells tall (title, secondary, padding) and headers are 2 cells tall (label, gap)
    // Viewport scrolling works on cumulative cell offsets so partial rows can't peek out at the top or bottom of the list
    // The clamp helper still treats one unit as one cell; we just pass cell offsets instead of line indices
    let lines = build_dashboard_lines(
        rows,
        grouping,
        &state.filter,
        &state.collapsed_sections,
        state.idle_show_all,
        state.search_mode,
    );
    let heights: Vec<u16> = lines
        .iter()
        .map(|l| match l {
            DashboardLine::Row(_) => ROW_HEIGHT,
            DashboardLine::Header { .. }
            | DashboardLine::PinnedHeader { .. }
            | DashboardLine::IdleOverflow { .. }
            | DashboardLine::Divider => GROUP_HEADER_HEIGHT,
        })
        .collect();
    let total_cells: usize = heights.iter().map(|h| *h as usize).sum();

    // Compute the selected row's `(top_cell, height)`
    // The viewport clamp keeps the WHOLE row in view rather than just its top cell
    let mut selected_cell: Option<(usize, u16)> = None;
    {
        let mut cum = 0usize;
        for (line, &h) in lines.iter().zip(heights.iter()) {
            // The cursor is whichever of the three targets is active: a row, or a section header (the button lives outside the list)
            let is_cursor = match line {
                DashboardLine::Row(r) => state.selected.as_ref().is_some_and(|s| *s == r.id),
                DashboardLine::PinnedHeader { .. } => {
                    state.selected_section == Some(SectionKey::Pinned)
                }
                DashboardLine::Header { state: rs, .. } => {
                    state.selected_section == Some(SectionKey::State(*rs))
                }
                DashboardLine::IdleOverflow { .. } => state.selected_idle_overflow,
                DashboardLine::Divider => false,
            };
            if is_cursor {
                selected_cell = Some((cum, h));
                break;
            }
            cum += h as usize;
        }
    }
    let viewport_h = area.height as usize;
    let snap_target = selected_cell.map(|(top, h)| top + (h as usize).saturating_sub(1));
    let offset = state.clamp_viewport(snap_target, viewport_h, total_cells);
    // Bias the offset up if the selection's TOP cell ended up above the visible window. The clamp only
    // guarantees the bottom edge of the selection is in range when snap_target was used.
    let offset = if !state.manual_scroll_active
        && let Some((sel_top, _)) = selected_cell
        && sel_top < offset
    {
        state.viewport_offset = sel_top;
        sel_top
    } else {
        offset
    };

    // Snap the offset DOWN to the nearest line boundary `render_row` paints title and secondary
    // starting at `rect.y`. A partial clip at the top would show the title (cell 0) where the gap
    // (cell 2) belongs: the row "sticks" to the top instead of scrolling away.
    let offset = snap_offset_to_line_boundary(offset, &heights);
    state.viewport_offset = offset;

    let needs_scrollbar = total_cells > viewport_h && area.width >= 4;
    // Overlay the scrollbar on the right edge rather than reserving a column, so showing or hiding it never shifts the row layout
    // Rows always paint at full width; the thumb sits on the trailing margin column
    let body_width = area.width;
    let max_y = area.y + area.height;

    // Content background per visible line (`None` means spacer), consumed by the half-block halo pass after the items are painted
    let mut line_bg: Vec<Option<Color>> = vec![None; viewport_h];

    let mut cell_y: usize = 0;
    for (line, &h) in lines.iter().zip(heights.iter()) {
        let next_cell_y = cell_y + h as usize;
        // Skip items entirely above the viewport.
        if next_cell_y <= offset {
            cell_y = next_cell_y;
            continue;
        }
        // Stop once we've painted past the visible window.
        if cell_y >= offset + viewport_h {
            break;
        }
        // Item start y (relative to the area top), accounting for the part that may be clipped above the viewport
        let visible_top = cell_y.max(offset);
        let y = area.y + (visible_top - offset) as u16;
        if y >= max_y {
            break;
        }
        let item_height = (next_cell_y - visible_top) as u16;
        let render_h = item_height.min(max_y - y);
        let line_rect = Rect {
            x: area.x,
            y,
            width: body_width,
            height: render_h,
        };
        // `line_bg` records each visible line's CONTENT background (`None` means spacer line) for the half-block halo pass below
        let mark = |line_bg: &mut Vec<Option<Color>>, dy: u16, bg: Color| {
            let idx = (y - area.y + dy) as usize;
            if let Some(slot) = line_bg.get_mut(idx) {
                *slot = Some(bg);
            }
        };
        match line {
            DashboardLine::PinnedHeader { count } => {
                let key = SectionKey::Pinned;
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header(
                    buf, line_rect, theme, "Pinned", *count, collapsed, selected, hovered,
                );
                mark(&mut line_bg, 0, theme.bg_base);
                // Full-height hit rect (label and trailing gap): no hover/click dead zone between items
                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, render_h)));
            }
            DashboardLine::Divider => {
                render_divider(buf, line_rect, theme);
                mark(&mut line_bg, 0, theme.bg_base);
            }
            DashboardLine::Header { state: rs, count } => {
                // Headers only paint into the first cell; the trailing gap stays at bg_base
                let key = SectionKey::State(*rs);
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header(
                    buf,
                    line_rect,
                    theme,
                    rs.group_label(),
                    *count,
                    collapsed,
                    selected,
                    hovered,
                );
                mark(&mut line_bg, 0, theme.bg_base);
                // Full-height hit rect (label and trailing gap): no hover/click dead zone between items
                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, render_h)));
            }
            DashboardLine::Row(row) => {
                render_row(buf, line_rect, theme, row, state);
                let bg = row_bg(theme, state, row);
                let content_top = row_content_offset(render_h, row);
                let content_h = row_content_height(row).min(render_h);
                for dy in content_top..(content_top + content_h).min(render_h) {
                    mark(&mut line_bg, dy, bg);
                }
                if !row.is_more_placeholder {
                    // Full-height hit rect (content and spacer lines): no hover/click dead zone between items
                    // The highlight covers the content plus half-cell halos on the neighbouring spacer lines
                    let hit = Rect {
                        x: area.x,
                        y,
                        width: body_width,
                        height: render_h,
                    };
                    state.row_rects.push((row.id.clone(), hit));
                }
            }
            DashboardLine::IdleOverflow { hidden, expanded } => {
                render_idle_overflow(
                    buf,
                    line_rect,
                    theme,
                    *hidden,
                    *expanded,
                    state.selected_idle_overflow,
                    state.hovered_idle_overflow,
                );
                mark(&mut line_bg, 0, theme.bg_base);
                // Full-height hit rect (label and trailing gap): no hover/click dead zone below the overflow row
                state.idle_overflow_rect = Some(Rect::new(area.x, y, body_width, render_h));
            }
        }
        cell_y = next_cell_y;
    }

    render_spacer_halos(buf, area, body_width, &line_bg, theme.bg_base);

    if needs_scrollbar {
        render_scrollbar(buf, area, offset, viewport_h, total_cells, theme);
    }
}

/// Paint the spacer lines between items as half-cell "halos" so a highlighted row reads as
/// vertically centered.
fn render_spacer_halos(
    buf: &mut Buffer,
    area: Rect,
    body_width: u16,
    line_bg: &[Option<Color>],
    base: Color,
) {
    for (i, slot) in line_bg.iter().enumerate() {
        if slot.is_some() {
            continue;
        }
        let above = i
            .checked_sub(1)
            .and_then(|j| line_bg.get(j))
            .copied()
            .flatten()
            .unwrap_or(base);
        let below = line_bg.get(i + 1).copied().flatten().unwrap_or(base);
        if above == base && below == base {
            continue;
        }
        let y = area.y + i as u16;
        if above == below {
            let fill = " ".repeat(body_width as usize);
            buf.set_string(area.x, y, &fill, Style::default().bg(above));
        } else {
            // A `Reset` fg on `▀` renders as the terminal's default *text*
            // color — a sharp bright bar on the terminal theme's canvas —
            // so skip the halo next to a Reset background.
            if matches!(above, Color::Reset) || matches!(below, Color::Reset) {
                continue;
            }
            let fill = "\u{2580}".repeat(body_width as usize);
            buf.set_string(area.x, y, &fill, Style::default().fg(above).bg(below));
        }
    }
}

/// Wide-mode group header reads.
#[allow(clippy::too_many_arguments)]
fn render_group_header(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    label: &str,
    count: usize,
    collapsed: bool,
    selected: bool,
    hovered: bool,
) {
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg);
    if rect.width == 0 {
        return;
    }
    // Selected (keyboard cursor) uses accent_user; hovered (mouse) uses the brighter text_primary; otherwise muted (dim gray, or the DIM attribute on the terminal theme where gray is Reset)
    let label_style = if selected {
        Style::default().fg(theme.accent_user)
    } else if hovered {
        Style::default().fg(theme.text_primary)
    } else {
        theme.muted()
    }
    .bg(theme.bg_base)
    .add_modifier(Modifier::BOLD);
    let count_str = format!(" {count}");
    let count_style = theme.dim().bg(theme.bg_base);
    let rule_style = Style::default()
        .fg(theme.selection_border)
        .bg(theme.bg_base);

    // Disclosure indicator: ▾ expanded, ▸ collapsed.
    let glyph_str = format!(
        "{} ",
        if collapsed {
            crate::glyphs::disclosure_closed()
        } else {
            crate::glyphs::disclosure_open()
        }
    );
    let glyph_w = UnicodeWidthStr::width(glyph_str.as_str()) as u16;
    let label_w = UnicodeWidthStr::width(label) as u16;
    let count_w = UnicodeWidthStr::width(count_str.as_str()) as u16;

    // Layout: disclosure glyph, then title text, then count and rule
    let mut cx = rect.x;
    if glyph_w >= rect.width {
        return;
    }
    buf.set_string(cx, rect.y, &glyph_str, label_style);
    cx += glyph_w;

    let label_avail = (rect.x + rect.width).saturating_sub(cx);
    if label_w >= label_avail {
        let trunc = truncate_str(label, label_avail as usize);
        buf.set_string(cx, rect.y, trunc, label_style);
        return;
    }
    buf.set_string(cx, rect.y, label, label_style);
    cx += label_w;

    if cx + count_w >= rect.x + rect.width {
        return;
    }
    buf.set_string(cx, rect.y, &count_str, count_style);
    cx += count_w;

    // Single-cell pad between count and rule so the digits don't bleed into the line
    let pad = 1u16;
    if cx + pad >= rect.x + rect.width {
        return;
    }
    cx += pad;

    let rule_w = (rect.x + rect.width).saturating_sub(cx);
    if rule_w == 0 {
        return;
    }
    let rule: String = "\u{2500}".repeat(rule_w as usize);
    buf.set_string(cx, rect.y, &rule, rule_style);
}

/// Textless divider: a full-width horizontal rule in the section-header rule style.
/// It is used when grouping is OFF to separate the pinned block from the rest without a labelled header.
/// Paints only the first cell; any trailing cell of its rect stays at `bg_base` (the breathing gap).
fn render_divider(buf: &mut Buffer, rect: Rect, theme: &Theme) {
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg);
    if rect.width == 0 {
        return;
    }
    let rule_style = Style::default()
        .fg(theme.selection_border)
        .bg(theme.bg_base);
    let rule: String = "\u{2500}".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, &rule, rule_style);
}

/// The Idle group's "N more" overflow toggle row: a single dim line painted at the bottom of a
/// capped Idle group.
fn render_idle_overflow(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    hidden: usize,
    expanded: bool,
    selected: bool,
    hovered: bool,
) {
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg);
    if rect.width == 0 {
        return;
    }
    let style = if selected {
        Style::default().fg(theme.accent_user)
    } else if hovered {
        Style::default().fg(theme.text_primary)
    } else {
        theme.dim()
    }
    .bg(theme.bg_base);
    let label = if expanded {
        "show fewer".to_string()
    } else {
        format!("{hidden} more")
    };
    // A `+` / `-` expand indicator in the icon column and the label in the agent-name column, so the row aligns with the Idle rows above
    // Columns: marker (1) + gap (1) + icon + gap (1); the Idle group is top-level, so indent is 0
    let indicator = if expanded { "-" } else { "+" };
    let icon_w = unicode_width::UnicodeWidthStr::width(state_icon(RowState::Idle, 0)) as u16;
    let indicator_x = rect.x.saturating_add(2);
    let name_x = indicator_x.saturating_add(icon_w + 1);
    if indicator_x < rect.x + rect.width {
        buf.set_string(indicator_x, rect.y, indicator, style);
    }
    if name_x >= rect.x + rect.width {
        return;
    }
    let avail = (rect.x + rect.width).saturating_sub(name_x);
    let trunc = truncate_str(&label, avail as usize);
    buf.set_string(name_x, rect.y, trunc, style);
}

/// Narrow-mode group header. Compact `Done 12` form, with no trailing rule because the narrow layout doesn't have the width budget.
#[allow(clippy::too_many_arguments)]
fn render_group_header_narrow(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    label: &str,
    count: usize,
    collapsed: bool,
    selected: bool,
    hovered: bool,
) {
    let bg_style = Style::default().bg(theme.bg_base);
    let label_style = if selected {
        Style::default().fg(theme.accent_user)
    } else if hovered {
        Style::default().fg(theme.text_primary)
    } else {
        theme.muted()
    }
    .bg(theme.bg_base)
    .add_modifier(Modifier::BOLD);
    let fill = " ".repeat(rect.width as usize);
    buf.set_string(rect.x, rect.y, fill, bg_style);
    let glyph = if collapsed {
        crate::glyphs::disclosure_closed()
    } else {
        crate::glyphs::disclosure_open()
    };
    let line = format!("{glyph} {label} {count}");
    let trunc = truncate_str(&line, rect.width as usize);
    buf.set_string(rect.x, rect.y, trunc, label_style);
}

/// The list content is inset by `LIST_OUTER_HPAD`; the scrollbar lives in that right margin so it
/// never reserves width from the rows.
fn render_scrollbar(
    buf: &mut Buffer,
    area: Rect,
    offset: usize,
    visible: usize,
    total: usize,
    theme: &Theme,
) {
    use crate::render::scrollbar::render_scrollbar_styled;

    let x = (area.x + area.width - 1 + super::layout::LIST_OUTER_HPAD)
        .min(buf.area.right().saturating_sub(1));
    let scrollbar_area = Rect {
        x,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let track_style = Style::default().bg(theme.scrollbar_bg);
    let thumb_style = Style::default()
        .fg(theme.scrollbar_fg)
        .bg(theme.scrollbar_bg);
    let cap = |v: usize| v.min(u16::MAX as usize) as u16;
    render_scrollbar_styled(
        buf,
        Some(scrollbar_area),
        cap(total),
        cap(visible),
        cap(offset),
        track_style,
        thumb_style,
    );
}

/// Snap a cell-granular viewport offset DOWN to the nearest item-start boundary in `heights`. This
/// exists because `render_row` paints its content from `rect.y` (title, then secondary) and has no
/// notion of a partial top clip. Visually the row "sticks" instead of scrolling away.
fn snap_offset_to_line_boundary(offset: usize, heights: &[u16]) -> usize {
    let mut cum = 0usize;
    let mut snapped = 0usize;
    for &h in heights {
        if cum > offset {
            break;
        }
        snapped = cum;
        cum = cum.saturating_add(h as usize);
    }
    snapped
}

/// Number of content lines a row renders: the title plus an optional secondary line.
fn row_content_height(row: &DashboardRow) -> u16 {
    if row.secondary_line.as_deref().is_some_and(|s| !s.is_empty()) {
        2
    } else {
        1
    }
}

/// Vertical offset of a row's content block within its rect.
/// The 1- or 2-line content is centered at cell granularity.
/// A title-only row in a 3-cell rect gets one padding line above and below, while a row with a secondary line stays top-aligned ((3 - 2) / 2 = 0).
fn row_content_offset(height: u16, row: &DashboardRow) -> u16 {
    height.saturating_sub(row_content_height(row)) / 2
}

/// The row's background: keyboard selection wins over mouse hover.
fn row_bg(theme: &Theme, state: &DashboardState, row: &DashboardRow) -> Color {
    if state.selected.as_ref().is_some_and(|s| *s == row.id) {
        theme.bg_highlight
    } else if state.hovered_row.as_ref().is_some_and(|h| *h == row.id) {
        theme.bg_hover
    } else {
        theme.bg_base
    }
}

/// Dim metadata (subtitle, secondary line) over the row background, via `Theme::dim()`.
/// That is a `gray_dim` fg on RGB themes, and the polarity-safe DIM attribute on the terminal theme — where `gray_dim` is the same bright black as the hover/selection band and would render the text invisible.
fn row_dim_style(theme: &Theme, bg: Color) -> Style {
    theme.dim().bg(bg)
}

/// A title-only row in a 3-cell rect renders as padding, title, padding.
fn render_row(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    row: &DashboardRow,
    state: &mut DashboardState,
) {
    if rect.area() == 0 {
        return;
    }
    let selected = state.selected.as_ref().is_some_and(|s| *s == row.id);
    let renaming = state.rename.as_ref().is_some_and(|r| r.row == row.id);
    let bg = row_bg(theme, state, row);

    // Paint the content lines with the row background
    // Spacer lines keep `bg_base` here; the halo pass splits them between the neighbouring items
    let content_top = row_content_offset(rect.height, row);
    let content_h = row_content_height(row).min(rect.height);
    let fill = " ".repeat(rect.width as usize);
    for dy in content_top..(content_top + content_h).min(rect.height) {
        buf.set_string(rect.x, rect.y + dy, &fill, Style::default().bg(bg));
    }

    // Layout columns: marker (1) | gap (1) | indent (2*n) | icon (1-2)
    //                | gap (1) | label/secondary start.
    let marker = if selected {
        crate::glyphs::selection_bar()
    } else {
        " "
    };
    let marker_w = UnicodeWidthStr::width(marker) as u16;
    let indent_w = (row.indent as u16) * 2;
    let icon = state_icon(row.state, state.spinner_tick);
    let icon_color = if row.state == RowState::NeedsInput {
        needs_input_bullet_color(state.spinner_tick, theme)
    } else {
        state_color(row.state, theme)
    };
    match row.state.animation() {
        Some(Animation::Spinner) => state.painted_animations.mark(Animation::Spinner),
        Some(Animation::Blink) if needs_input_blink_visible(theme) => {
            state.painted_animations.mark(Animation::Blink);
        }
        Some(Animation::Blink) | None => {}
    }
    let icon_w = UnicodeWidthStr::width(icon) as u16;
    // Title-row paint cursor. Title-only rows sit padded above and below, while 2-line rows stay
    // top-aligned (2 lines cannot center in a 3-cell row).
    let title_y = rect.y + row_content_offset(rect.height, row);
    let content_start_x = rect.x + marker_w + 1 + indent_w + icon_w + 1;

    // Rename overlay: keep the row's chrome (marker and state icon) in place and swap ONLY the title text for `rename: {draft}`
    // It is painted at the title's own column so the row stays visually aligned with its neighbours while editing
    if renaming && let Some(rn) = state.rename.as_ref() {
        buf.set_string(
            rect.x,
            title_y,
            marker,
            Style::default()
                .bg(bg)
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        );
        // Keep the left bar continuous on every content line even while the rename overlay is active on the title line
        if selected {
            let bar_style = Style::default()
                .bg(bg)
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD);
            for dy in content_top..(content_top + content_h).min(rect.height) {
                buf.set_string(
                    rect.x,
                    rect.y + dy,
                    crate::glyphs::selection_bar(),
                    bar_style,
                );
            }
        }
        // State icon stays put (same column and colour as the normal row)
        buf.set_string(
            rect.x + marker_w + 1 + indent_w,
            title_y,
            icon,
            Style::default().fg(icon_color).bg(bg),
        );
        let available = (rect.x + rect.width).saturating_sub(content_start_x);
        render_rename_editor(
            buf,
            content_start_x,
            title_y,
            available,
            Style::default()
                .fg(theme.accent_user)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
            rn,
        );
        return;
    }

    // Marker and icon
    buf.set_string(
        rect.x,
        title_y,
        marker,
        Style::default()
            .bg(bg)
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD),
    );

    // For the active selection, extend the thin left bar down every content line of the row
    // It forms one continuous vertical rule along the highlighted text
    // Hover and normal states keep their marker only on the title line
    if selected {
        let bar_style = Style::default()
            .bg(bg)
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD);
        for dy in content_top..(content_top + content_h).min(rect.height) {
            buf.set_string(
                rect.x,
                rect.y + dy,
                crate::glyphs::selection_bar(),
                bar_style,
            );
        }
    }

    let icon_x = rect.x + marker_w + 1 + indent_w;
    buf.set_string(
        icon_x,
        title_y,
        icon,
        Style::default().bg(bg).fg(icon_color),
    );

    let armed_delete = state.armed_delete_row_ref();
    let show_delete = !row.is_more_placeholder
        && !row.id.is_subagent()
        && (!row.id.is_workspace() || state.workspace_membership_mode)
        && row.state.allows_delete()
        && !state.row_is_conversation(&row.id)
        && (state.hovered_row.as_ref() == Some(&row.id) || armed_delete == Some(&row.id));
    let delete_label = crate::glyphs::ballot_x_button();
    let delete_w = UnicodeWidthStr::width(delete_label) as u16;
    let age = format_time_ago(row.last_change_at.elapsed().unwrap_or_default());
    let age_w = UnicodeWidthStr::width(age.as_str()) as u16;
    // Each row pins its own age to the right edge. Chips, when present, sit
    // immediately left of ` · <age>`. Working rows never swap the age for delete.
    let meta_w = if show_delete { delete_w } else { age_w };
    let meta_x = rect.x + rect.width.saturating_sub(meta_w.saturating_add(1));
    let time_sep = " · ";
    let time_sep_w = UnicodeWidthStr::width(time_sep) as u16;
    let join_time = crate::views::dashboard::row_title::has_counted_live_work(&row.badges);
    let title_gap = if join_time { time_sep_w } else { 2 };
    let title_w = meta_x
        .saturating_sub(content_start_x)
        .saturating_sub(title_gap);
    let chip_w = RowTitle { row, theme, bg }
        .render_wide(buf, Rect::new(content_start_x, title_y, title_w, 1));
    if chip_w > 0 && meta_x >= content_start_x.saturating_add(time_sep_w) {
        buf.set_string(
            meta_x.saturating_sub(time_sep_w),
            title_y,
            time_sep,
            Style::default().bg(bg).fg(theme.gray),
        );
    }
    if meta_x > content_start_x {
        if show_delete {
            let fg = if state.hovered_delete.as_ref() == Some(&row.id)
                || armed_delete == Some(&row.id)
            {
                theme.accent_error
            } else {
                theme.text_secondary
            };
            buf.set_string(
                meta_x,
                title_y,
                delete_label,
                Style::default().bg(bg).fg(fg),
            );
            state.row_delete_rects.push((
                row.id.clone(),
                Rect {
                    x: meta_x,
                    y: title_y,
                    width: delete_w,
                    height: 1,
                },
            ));
        } else {
            buf.set_string(
                meta_x,
                title_y,
                &age,
                Style::default().bg(bg).fg(theme.gray),
            );
        }
    }

    // Secondary row
    // The selected row brightens its secondary line so the user can read what the agent is doing right now without leaving the dashboard
    // Unselected rows keep the dimmer `gray_dim` so they read as the row's metadata tail rather than competing with the title
    if rect.height >= 2
        && let Some(secondary) = row.secondary_line.as_deref()
        && !secondary.is_empty()
    {
        let sec_y = title_y + 1;
        let avail = rect
            .width
            .saturating_sub(content_start_x - rect.x)
            .saturating_sub(1);
        if avail > 0 {
            let trunc = truncate_str(secondary, avail as usize);
            let secondary_style = if selected {
                Style::default().bg(bg).fg(theme.text_secondary)
            } else {
                row_dim_style(theme, bg)
            };
            // The awaiting-input subtitle is `Pending: …`
            // Paint the `Pending:` prefix in yellow so the actionable state stands out, and the rest in the normal secondary colour
            const PENDING_PREFIX: &str = "Pending:";
            if let Some(rest) = trunc.strip_prefix(PENDING_PREFIX) {
                buf.set_string(
                    content_start_x,
                    sec_y,
                    PENDING_PREFIX,
                    Style::default().bg(bg).fg(theme.warning),
                );
                let prefix_w = UnicodeWidthStr::width(PENDING_PREFIX) as u16;
                buf.set_string(content_start_x + prefix_w, sec_y, rest, secondary_style);
            } else {
                buf.set_string(content_start_x, sec_y, trunc, secondary_style);
            }
        }
    }

    // Terminal theme (Reset band slots): the selection/hover cue is reverse
    // video over the content lines. RGB themes keep their baked band.
    if theme.is_bandless() {
        let hovered = state.hovered_row.as_ref().is_some_and(|h| *h == row.id);
        // Skip while renaming (editable line), like the narrow path.
        if (selected || hovered) && !renaming {
            let content = Rect {
                x: rect.x,
                y: rect.y + row_content_offset(rect.height, row),
                width: rect.width,
                height: row_content_height(row).min(rect.height),
            };
            // Normalize fgs first: colored glyphs (state symbol, the
            // Pending badge) would invert into colored background patches;
            // on the band they take the same default fg as the text.
            crate::render::color::force_area_fg(buf, content, Color::Reset);
            buf.set_style(
                content,
                Style::default().add_modifier(ratatui::style::Modifier::REVERSED),
            );
        }
    }
}

#[cfg(test)]
fn render_narrow_rows(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    state: &mut DashboardState,
) {
    render_narrow_rows_with_grouping(buf, area, theme, rows, state.grouping, state);
}

fn render_narrow_rows_with_grouping(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    rows: &[DashboardRow],
    grouping: Grouping,
    state: &mut DashboardState,
) {
    state.row_rects.clear();
    state.row_delete_rects.clear();
    state.section_rects.clear();
    state.idle_overflow_rect = None;
    if area.area() == 0 {
        return;
    }

    // Narrow mode stays single-line per row (the wide path's 2-line form would push too many rows off-screen on a 40-col terminal)
    // We still emit group headers and the selection marker so the visual vocabulary stays consistent
    let lines = build_dashboard_lines(
        rows,
        grouping,
        &state.filter,
        &state.collapsed_sections,
        state.idle_show_all,
        state.search_mode,
    );
    let viewport_h = area.height as usize;
    // The clamp follows whichever cursor is active, a row or a section header
    // Navigating onto a section title therefore scrolls it into view, matching the wide layout
    let selected_line_idx = lines.iter().position(|l| match l {
        DashboardLine::Row(r) => state.selected.as_ref().is_some_and(|s| r.id == *s),
        DashboardLine::PinnedHeader { .. } => state.selected_section == Some(SectionKey::Pinned),
        DashboardLine::Header { state: rs, .. } => {
            state.selected_section == Some(SectionKey::State(*rs))
        }
        DashboardLine::IdleOverflow { .. } => state.selected_idle_overflow,
        DashboardLine::Divider => false,
    });
    let offset = state.clamp_viewport(selected_line_idx, viewport_h, lines.len());

    let needs_scrollbar = lines.len() > viewport_h && area.width >= 4;
    // Overlay the scrollbar (see `render_rows`): no reserved column, no shift
    let body_width = area.width;

    let mut y = area.y;
    for line in lines.iter().skip(offset).take(viewport_h) {
        if y >= area.y + area.height {
            break;
        }
        let line_rect = Rect {
            x: area.x,
            y,
            width: body_width,
            height: 1,
        };
        let row = match line {
            DashboardLine::PinnedHeader { count } => {
                let key = SectionKey::Pinned;
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header_narrow(
                    buf, line_rect, theme, "Pinned", *count, collapsed, selected, hovered,
                );
                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, 1)));
                y += 1;
                continue;
            }
            DashboardLine::Header { state: rs, count } => {
                let key = SectionKey::State(*rs);
                let collapsed = state.is_section_collapsed(key);
                let selected = state.selected_section == Some(key);
                let hovered = state.hovered_section == Some(key);
                render_group_header_narrow(
                    buf,
                    line_rect,
                    theme,
                    rs.group_label(),
                    *count,
                    collapsed,
                    selected,
                    hovered,
                );
                state
                    .section_rects
                    .push((key, Rect::new(area.x, y, body_width, 1)));
                y += 1;
                continue;
            }
            DashboardLine::Divider => {
                render_divider(buf, line_rect, theme);
                y += 1;
                continue;
            }
            DashboardLine::IdleOverflow { hidden, expanded } => {
                render_idle_overflow(
                    buf,
                    line_rect,
                    theme,
                    *hidden,
                    *expanded,
                    state.selected_idle_overflow,
                    state.hovered_idle_overflow,
                );
                state.idle_overflow_rect = Some(Rect::new(area.x, y, body_width, 1));
                y += 1;
                continue;
            }
            DashboardLine::Row(row) => row,
        };

        let selected = state.selected.as_ref().is_some_and(|s| *s == row.id);
        let hovered = state.hovered_row.as_ref().is_some_and(|h| *h == row.id);
        let renaming = state.rename.as_ref().is_some_and(|r| r.row == row.id);
        let bg = if selected {
            theme.bg_highlight
        } else if hovered {
            theme.bg_hover
        } else {
            theme.bg_base
        };

        if row.state == RowState::Working {
            state.painted_animations.mark(Animation::Spinner);
        }
        if renaming && let Some(rn) = state.rename.as_ref() {
            // Mirror the wide layout: keep the marker and state icon chrome and swap only the label for `rename: {draft}`
            // The editing row then stays column-aligned with its neighbours
            let marker = if selected {
                crate::glyphs::selection_bar()
            } else {
                " "
            };
            let icon = state_icon(row.state, state.spinner_tick);
            let indent = "  ".repeat(row.indent as usize);
            let chrome = format!("{marker} {indent}{icon} ");
            let chrome_w = UnicodeWidthStr::width(chrome.as_str()) as u16;
            buf.set_string(
                area.x,
                y,
                &chrome,
                Style::default().fg(theme.text_primary).bg(bg),
            );
            render_rename_editor(
                buf,
                area.x + chrome_w,
                y,
                body_width.saturating_sub(chrome_w),
                Style::default()
                    .fg(theme.accent_user)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
                rn,
            );
        } else {
            let marker = if selected {
                crate::glyphs::selection_bar()
            } else {
                " "
            };
            let marker_w = UnicodeWidthStr::width(marker) as u16;
            let icon = state_icon(row.state, state.spinner_tick);
            let icon_w = UnicodeWidthStr::width(icon) as u16;
            let indent = "  ".repeat(row.indent as usize);
            let indent_w = UnicodeWidthStr::width(indent.as_str()) as u16;
            let gap_after_marker = 1u16;
            let chrome = marker_w + gap_after_marker + indent_w + icon_w + 1;
            let armed_here = state.armed_delete_row_ref() == Some(&row.id);
            let show_delete = !row.is_more_placeholder
                && !row.id.is_subagent()
                && (!row.id.is_workspace() || state.workspace_membership_mode)
                && row.state.allows_delete()
                && !state.row_is_conversation(&row.id)
                && (hovered || armed_here);
            let delete_label = crate::glyphs::ballot_x_button();
            let delete_w = UnicodeWidthStr::width(delete_label) as u16;
            let label_budget = body_width
                .saturating_sub(chrome)
                .saturating_sub(if show_delete { delete_w + 1 } else { 0 });
            let line = format!("{marker} {indent}{icon} ");
            buf.set_string(
                area.x,
                y,
                line,
                Style::default().fg(theme.text_primary).bg(bg),
            );
            RowTitle { row, theme, bg }
                .render_narrow(buf, Rect::new(area.x + chrome, y, label_budget, 1));
            if show_delete && body_width > chrome + delete_w {
                let dx = area.x + body_width.saturating_sub(delete_w);
                let fg = if state.hovered_delete.as_ref() == Some(&row.id) || armed_here {
                    theme.accent_error
                } else {
                    theme.text_secondary
                };
                buf.set_string(dx, y, delete_label, Style::default().fg(fg).bg(bg));
                state
                    .row_delete_rects
                    .push((row.id.clone(), Rect::new(dx, y, delete_w, 1)));
            }
        }
        // Terminal theme (Reset band slots): same uniform reverse-video cue
        // as the wide rows. Skip while renaming (editable line).
        if theme.is_bandless() && (selected || hovered) && !renaming {
            crate::render::color::force_area_fg(buf, line_rect, Color::Reset);
            buf.set_style(
                line_rect,
                Style::default().add_modifier(ratatui::style::Modifier::REVERSED),
            );
        }
        if !row.is_more_placeholder {
            state.row_rects.push((row.id.clone(), line_rect));
        }
        y += 1;
    }

    if needs_scrollbar {
        render_scrollbar(buf, area, offset, viewport_h, lines.len(), theme);
    }
}

/// Rendered when agents exist but the filter has hidden every row.
/// Distinct from the empty-state hint so the user knows their filter is what's hiding the rows.
fn render_no_match(buf: &mut Buffer, area: Rect, theme: &Theme, filter: &Filter) {
    if area.area() == 0 {
        return;
    }
    let hint = match filter {
        Filter::None => "No matching rows.".to_string(),
        Filter::Agent(n) => format!("No agents match `a:{n}`. Press Esc to clear the filter."),
        Filter::State(s) => format!(
            "No agents in state `{}`: press Esc to clear the filter.",
            s.group_label()
        ),
        Filter::Substring(n) => format!("No rows match `{n}`: press Esc to clear the filter."),
    };
    let truncated = truncate_str(&hint, area.width.saturating_sub(2) as usize);
    // Explicit offset to avoid `area.y + 1.min(...)` precedence ambiguity
    // Drop one row of padding when the area can accommodate it; otherwise stay at the top
    let y_offset: u16 = if area.height >= 2 { 1 } else { 0 };
    buf.set_string(
        area.x + 1,
        area.y + y_offset,
        truncated,
        Style::default().fg(theme.gray),
    );
}

fn render_empty_state(buf: &mut Buffer, area: Rect, theme: &Theme, loading: bool) {
    if area.area() == 0 {
        return;
    }
    // A single dim line: the dispatch input below is the call to action, so no multi-line onboarding is needed (but never render a blank screen)
    // While the local session roster is being fetched we show a loading hint so a fresh open doesn't flash the "no agents" copy before rows land
    let line = if loading {
        "Loading sessions…"
    } else {
        "No agents yet, type a prompt to start one."
    };
    let truncated = truncate_str(line, area.width.saturating_sub(2) as usize);
    // See `render_no_match` for the precedence rationale.
    let y_offset: u16 = if area.height >= 2 { 1 } else { 0 };
    buf.set_string(
        area.x + 1,
        area.y + y_offset,
        truncated,
        Style::default().fg(theme.gray),
    );
}

/// On a 1-row rect (very short terminals) we fall back to the bare `❯ {text}` line so the input
/// stays usable. Examples: `✗ Session no longer exists`, `✓ Theme: Grok Day`. The badge therefore
/// neither prepends a glyph nor forces a colour.
fn paint_dispatch_feedback_badge(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    error_toast: Option<&str>,
) {
    let Some(err) = error_toast else {
        return;
    };
    // Leave room for the two rounded corners plus a little breathing space, so the bar still reads as a border rather than a full banner
    let max_w = area.width.saturating_sub(4);
    if max_w < 6 {
        return;
    }
    // Leading/trailing spaces pad the chip away from the surrounding border glyphs
    // No glyph is prepended; the message already owns one (see the doc comment)
    let label = format!(" {err} ");
    let trunc = truncate_str(&label, max_w as usize);
    let label_w = UnicodeWidthStr::width(trunc.as_str()) as u16;
    // Right-align so the badge ends one column before the `╮` corner.
    let x = area.x + area.width.saturating_sub(1 + label_w);
    buf.set_string(
        x,
        area.y,
        &trunc,
        Style::default()
            .fg(theme.accent_user)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    );
}

/// The model name renders in dim secondary text; the mode shows as a `plan` (plan accent) or
/// `always-approve` (default) flag.
fn paint_dispatch_config_badge(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &DashboardState,
    input_focused: bool,
) {
    use crate::views::dashboard::DashboardDispatchMode;
    use crate::views::prompt_widget::{PromptFlag, PromptInfo};

    if area.height < 3 || area.width < 6 {
        return;
    }
    let model_label = state
        .pending_model
        .as_ref()
        .map(|m| match m.effort {
            Some(effort) => format!("{} ({effort})", m.display),
            None => m.display.clone(),
        })
        .or_else(|| state.models.current_model_name())
        .unwrap_or_default();

    // Mode flag, styled exactly like the chat prompt's mode flags.
    let mut flags: Vec<PromptFlag> = Vec::new();
    match state.pending_mode {
        DashboardDispatchMode::Plan => flags.push(PromptFlag {
            text: "plan",
            color: Some(theme.accent_plan),
            bold: false,
        }),
        DashboardDispatchMode::Auto => flags.push(PromptFlag {
            text: "auto",
            color: Some(theme.accent_system),
            bold: false,
        }),
        DashboardDispatchMode::AlwaysApprove => flags.push(PromptFlag {
            text: "always-approve",
            color: None,
            bold: false,
        }),
        DashboardDispatchMode::Normal => {}
    }

    if model_label.is_empty() && flags.is_empty() && !state.multiline_mode {
        return;
    }

    let info = PromptInfo {
        model_name: &model_label,
        flags: &flags,
        multiline: state.multiline_mode,
        usage_warning: None,
        usage_warning_critical: false,
        model_tiers: None,
    };
    // Bottom border row, inside the corners: the same content rect the chat prompt uses for its info line
    let info_rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - 1,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    state
        .dispatch
        .render_info_line(buf, info_rect, &info, theme.bg_base, theme, input_focused);
}

/// Paint the left-aligned `● rec` badge on a box's top border while the mic is hot.
/// Shared by the dispatch box and the peek panel that replaces it, so a capture started in either box shows the same indicator.
pub(super) fn paint_record_badge(buf: &mut Buffer, area: Rect, theme: &Theme, listening: bool) {
    if listening && area.width >= 12 {
        buf.set_string(
            area.x + 2,
            area.y,
            " \u{25CF} rec ",
            Style::default()
                .fg(theme.accent_error)
                .bg(theme.bg_base)
                .add_modifier(Modifier::BOLD),
        );
    }
}

fn render_dispatch(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &mut DashboardState,
    overlay_area: Option<Rect>,
) -> Option<(u16, u16)> {
    use ratatui::widgets::{Block, BorderType, Borders, Widget};

    use crate::views::prompt_widget::{PromptBg, PromptStyle};

    if area.area() == 0 {
        return None;
    }
    let bg = Style::default().bg(theme.bg_base);
    let fill = " ".repeat(area.width as usize);
    for dy in 0..area.height {
        buf.set_string(area.x, area.y + dy, &fill, bg);
    }

    // Two-focus model: when the overview list is focused (Tab), the input is inactive
    // Dim its border and suppress the caret so the focus cue is unambiguous; `input_focused` drives both
    let input_focused = !state.list_focused;
    let border_fg = if input_focused {
        theme.selection_border
    } else {
        theme.prompt_border
    };

    // Draw the rounded box and carve out the content rows
    // The content height tracks the box height so multiline input (Alt+Enter) is fully visible; the box grows via `compute_layout_with_dispatch`
    let content = if area.height >= 3 {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_fg).bg(theme.bg_base));
        let inner = block.inner(area);
        block.render(area, buf);
        // Show dispatch-validation feedback (e.g. "Too short") as a right-aligned badge on the box's top border.
        // It stays visible even while the rejected text is still in the input
        paint_dispatch_feedback_badge(buf, area, theme, state.error_toast.as_deref());
        // Bottom-right model and mode indicator, painted through the shared prompt info-line renderer
        // Its style, spacing, and position match the chat prompt's info line exactly
        // Always shows the model the next agent will use (the `/model`-staged choice, else the current default), plus the staged mode as a flag
        paint_dispatch_config_badge(buf, area, theme, state, input_focused);
        paint_record_badge(buf, area, theme, state.voice_listening);
        Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: inner.height.max(1),
        }
    } else {
        // Fallback: single line, no chrome
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        }
    };
    if content.width == 0 {
        return None;
    }

    // Search mode: the prompt is a live filter query, rendered on a single line with a bold yellow `Search:` prefix
    // The prefix makes it unmistakable that typing filters rows (Enter confirms) rather than dispatching
    // Chips and multiline are not rendered here
    if state.search_mode {
        let prefix = "Search: ";
        let prefix_w = UnicodeWidthStr::width(prefix) as u16;
        let painted_prefix_w = prefix_w.min(content.width);
        buf.set_span(
            content.x,
            content.y,
            &Span::styled(
                prefix,
                Style::default()
                    .fg(theme.warning)
                    .bg(theme.bg_base)
                    .add_modifier(Modifier::BOLD),
            ),
            painted_prefix_w,
        );
        let editor_x = content.x + painted_prefix_w;
        let avail = content.width - painted_prefix_w;
        let cursor_column = if state.dispatch.text().is_empty() {
            if avail > 0 {
                let placeholder = truncate_str("Type to filter sessions\u{2026}", avail as usize);
                buf.set_string(
                    editor_x,
                    content.y,
                    placeholder,
                    theme.dim().bg(theme.bg_base),
                );
            }
            0
        } else {
            let viewport = distill_ratatui_textarea::EditBuffer::from_parts(
                state.dispatch.text(),
                state.dispatch.cursor(),
            )
            .single_line_viewport(avail as usize);
            let visible = state
                .dispatch
                .text()
                .get(viewport.visible_byte_range.clone())
                .unwrap_or("");
            if avail > 0 {
                buf.set_span(
                    editor_x,
                    content.y,
                    &Span::styled(
                        visible,
                        Style::default().fg(theme.text_primary).bg(theme.bg_base),
                    ),
                    (UnicodeWidthStr::width(visible) as u16).min(avail),
                );
            }
            viewport.cursor_display_column as u16
        };
        let cursor_offset = painted_prefix_w
            .saturating_add(cursor_column)
            .min(content.width - 1);
        let cx = content.x + cursor_offset;
        if input_focused && let Some(cell) = buf.cell_mut((cx, content.y)) {
            cell.set_style(theme.block_cursor_over(theme.bg_base));
        }
        return input_focused.then_some((cx, content.y));
    }

    if content.width < 4 {
        return None;
    }

    let prefix = "\u{276F} ";
    let prefix_w = UnicodeWidthStr::width(prefix) as u16;

    // When voice is active, draw through PromptWidget even on an empty buffer so the manual empty-state branch is skipped
    let voice_overlay = (state.voice_listening || state.voice_interim.is_some()).then_some(
        crate::views::prompt_widget::VoicePromptOverlay {
            interim: state.voice_interim.as_deref(),
            color: theme.accent_running,
        },
    );

    // Empty input (non-search): paint the `❯` prefix and the contextual placeholder (reply / error toast / new-session)
    // Park the caret at the text start
    // `PromptWidget::draw` only paints a placeholder when *unfocused*, but we want a visible caret, so the empty state is rendered directly here
    if state.dispatch.text().is_empty() && voice_overlay.is_none() {
        buf.set_string(
            content.x,
            content.y,
            prefix,
            Style::default().fg(theme.accent_user).bg(theme.bg_base),
        );
        // The dispatch input always spawns a NEW session, never a reply, so the placeholder is constant
        // whatever row the overview cursor is on. It only paints while the input is UNFOCUSED (matching
        // `PromptWidget::draw`).
        if !input_focused {
            let msg = "Dispatch a new agent";
            let style = theme.dim().bg(theme.bg_base);
            let trunc = truncate_str(msg, content.width.saturating_sub(prefix_w) as usize);
            buf.set_string(content.x + prefix_w, content.y, trunc, style);
        }
        return input_focused.then_some((content.x + prefix_w, content.y));
    }

    // Non-empty input (non-search): shared PromptWidget for cursor, chips, multiline
    // `chrome: false` keeps the dashboard box; `image_preview: false` keeps image chips without an overlay
    let style = PromptStyle {
        focused: input_focused,
        show_prefix: true,
        vpad_top: 0,
        chrome: false,
        bg: PromptBg::Canvas(theme.bg_base),
        image_preview: false,
        ..PromptStyle::default()
    };
    state
        .dispatch
        .draw(buf, content, overlay_area, &style, None, voice_overlay)
        .cursor_pos
}

/// Desired number of *text* rows for the dispatch box given its content and the box's outer width.
/// Grows the box for multiline input (Alt+Enter) while capping growth so the row list keeps usable space.
/// Returns at least 1.
fn dispatch_text_rows(state: &DashboardState, dispatch_width: u16, area_height: u16) -> u16 {
    use crate::views::prompt_widget::PromptStyle;

    // Match `render_dispatch`'s content width: box width minus the two border columns and the 1-col inset on each side (`-4` total)
    let content_w = dispatch_width.saturating_sub(4);
    if content_w < 4 {
        return 1;
    }
    // Cap so the box never eats more than about a third of the panel; the textarea scrolls beyond that
    let max_text_rows = (area_height / 3).clamp(1, 8);
    let style = PromptStyle {
        focused: true,
        show_prefix: true,
        vpad_top: 0,
        chrome: false,
        ..PromptStyle::default()
    };
    state
        .dispatch
        .desired_height(content_w, &style, false, max_text_rows)
}

/// Top and bottom border rows of a dispatch dropdown panel.
const DROPDOWN_CHROME_ROWS: u16 = 2;

/// Smallest panel that still carries both borders and one item row.
const MIN_DROPDOWN_PANEL_ROWS: u16 = DROPDOWN_CHROME_ROWS + 1;

/// Render the `/command` completion dropdown above the dispatch box.
/// Mirrors `agent_view`'s slash dropdown chrome.
/// No-op (and clears the stored hit rect) when the dropdown is closed.
fn render_slash_dropdown(
    buf: &mut Buffer,
    area: Rect,
    dispatch_rect: Rect,
    theme: &Theme,
    state: &mut DashboardState,
) {
    use ratatui::widgets::{Clear, Widget};

    use crate::views::slash_dropdown::{desired_item_rows, render_dropdown as render_slash};

    let snap = state.dispatch.slash_snapshot();
    if !snap.open || snap.matches.is_empty() {
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        return;
    }

    let item_count = snap.matches.len();
    // Height in wrapped lines, not items (see `desired_item_rows`); items render inset 1 col on each side
    let item_rows = desired_item_rows(&snap.matches, dispatch_rect.width.saturating_sub(2));
    // The bottom is pinned to the input, so a panel taller than the space above it would start above `area` and paint off the buffer
    let panel_h = item_rows
        .saturating_add(DROPDOWN_CHROME_ROWS)
        .min(dispatch_rect.y.saturating_sub(area.y));
    if panel_h < MIN_DROPDOWN_PANEL_ROWS {
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        return;
    }
    let top_y = dispatch_rect.y - panel_h;
    let panel_x = dispatch_rect.x;
    let panel_width = dispatch_rect.width;
    if panel_width < 4 {
        state.slash_dropdown_items_area = None;
        state.slash_dropdown_hit = Default::default();
        return;
    }
    let panel_area = Rect {
        x: panel_x,
        y: top_y,
        width: panel_width,
        height: panel_h,
    };

    Clear.render(panel_area, buf);
    buf.set_style(
        panel_area,
        Style::default().fg(theme.text_primary).bg(theme.bg_light),
    );

    let border_style = Style::default()
        .fg(theme.panel_border_fg())
        .bg(theme.bg_base);
    let bar: String = "\u{2500}".repeat(panel_width as usize);
    buf.set_string(panel_x, top_y, &bar, border_style);
    buf.set_string(panel_x, top_y + panel_h - 1, &bar, border_style);

    let hint = format!("{item_count}");
    let hint_w = hint.len() as u16;
    if hint_w + 2 <= panel_width {
        let hint_x = panel_x + panel_width - hint_w - 1;
        buf.set_string(
            hint_x,
            top_y,
            &hint,
            Style::default().fg(theme.gray).bg(theme.bg_base),
        );
    }

    let items_x = panel_x + 1;
    let items_width = panel_width.saturating_sub(2);
    let items_area = Rect {
        x: items_x,
        y: top_y + 1,
        width: items_width,
        height: panel_h - DROPDOWN_CHROME_ROWS,
    };
    state.slash_dropdown_hit = render_slash(
        buf,
        items_area,
        &snap,
        state.dispatch.slash_hovered(),
        theme,
    );
    state.slash_dropdown_items_area = Some(items_area);
}

/// Working directory of the agent owning the peeked `row`, used to root the reply's `@` file picker.
/// Top-level rows use their own cwd; subagent rows reply to (and resolve `@paths` against) their parent; roster rows have no local agent (`None`).
/// `None` too when the agent has since vanished.
fn peeked_agent_cwd(
    row: &super::DashboardRowId,
    agents: &IndexMap<AgentId, AgentView>,
) -> Option<std::path::PathBuf> {
    let id = match row {
        super::DashboardRowId::TopLevel(id) => *id,
        super::DashboardRowId::Subagent { parent, .. } => *parent,
        super::DashboardRowId::Roster { .. } | super::DashboardRowId::Workspace { .. } => {
            return None;
        }
    };
    agents.get(&id).map(|a| a.session.cwd.clone())
}

/// Render the session-less `@` file-context picker dropdown above the dispatch box.
/// Twin of [`render_slash_dropdown`] with a `k/n` count hint.
/// No-op (and clears the hit rect) when the picker is hidden.
fn render_file_search_dropdown(
    buf: &mut Buffer,
    area: Rect,
    dispatch_rect: Rect,
    theme: &Theme,
    state: &mut DashboardState,
) {
    state.file_search_dropdown_items_area = render_file_search_dropdown_for(
        buf,
        area,
        dispatch_rect,
        theme,
        &mut state.dispatch.file_search,
    );
}

/// Paint a session-less `@` file-context dropdown ABOVE `anchor_rect` for the given [`FileSearchState`].
/// `anchor_rect` is the dispatch box, or the peek box when the reply is the active input.
/// Returns the items-area rect for mouse routing, or `None` when nothing was drawn (hidden, empty, or no room above the anchor).
fn render_file_search_dropdown_for(
    buf: &mut Buffer,
    area: Rect,
    anchor_rect: Rect,
    theme: &Theme,
    file_search: &mut crate::views::file_search::FileSearchState,
) -> Option<Rect> {
    use ratatui::widgets::{Clear, Widget};

    use crate::views::file_search::dropdown::{MAX_DROPDOWN_ROWS, render_dropdown as render_files};

    if !file_search.is_visible() {
        return None;
    }
    let item_count = file_search.result_count();
    let item_rows = (item_count as u16).min(MAX_DROPDOWN_ROWS);
    if item_rows == 0 {
        return None;
    }
    let panel_h = item_rows.saturating_add(2);
    let max_top = anchor_rect.y.saturating_sub(1);
    if max_top <= area.y {
        return None;
    }
    let top_y = max_top.saturating_sub(panel_h - 1);
    if top_y < area.y {
        return None;
    }
    let panel_x = anchor_rect.x;
    let panel_width = anchor_rect.width;
    if panel_width < 4 {
        return None;
    }

    // Keep the selected row inside the visible window before painting.
    file_search.ensure_visible(item_rows as usize);

    let panel_area = Rect {
        x: panel_x,
        y: top_y,
        width: panel_width,
        height: panel_h,
    };
    Clear.render(panel_area, buf);
    buf.set_style(
        panel_area,
        Style::default().fg(theme.text_primary).bg(theme.bg_light),
    );

    let border_style = Style::default()
        .fg(theme.panel_border_fg())
        .bg(theme.bg_base);
    let bar: String = "\u{2500}".repeat(panel_width as usize);
    buf.set_string(panel_x, top_y, &bar, border_style);
    buf.set_string(panel_x, top_y + panel_h - 1, &bar, border_style);

    let (k, n) = (file_search.result_count(), file_search.total_items());
    let hint = if k >= 1000 {
        format!("1k+/{n}")
    } else {
        format!("{k}/{n}")
    };
    let hint_w = hint.len() as u16;
    if hint_w + 2 <= panel_width {
        let hint_x = panel_x + panel_width - hint_w - 1;
        buf.set_string(
            hint_x,
            top_y,
            &hint,
            Style::default().fg(theme.gray).bg(theme.bg_base),
        );
    }

    let items_x = panel_x + 1;
    let items_width = panel_width.saturating_sub(2);
    let items_area = Rect {
        x: items_x,
        y: top_y + 1,
        width: items_width,
        height: item_rows,
    };
    render_files(buf, items_area, file_search, theme);
    Some(items_area)
}

/// There is no inline approve/reject yet; the dashboard is intentionally a navigator, not a
/// permission UI. The. CtrlCtrl+x chip label follows the selected agent's state: `stop` for an agent
/// with a live turn, `delete` otherwise.
#[allow(clippy::too_many_arguments)]
fn render_footer(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    state: &DashboardState,
    registry: &crate::actions::ActionRegistry,
    selected_state: Option<RowState>,
    peek_active: bool,
    pending_hint: Option<crate::views::shortcuts_bar::PendingHint>,
) {
    use ratatui::widgets::Widget;

    use crate::input::key::{KeyShortcut, key};
    use crate::views::shortcuts_bar::{HintItem, PendingHint, ShortcutsBar};

    if area.area() == 0 {
        return;
    }

    // 2-col left padding to match the agent view's footer position (`block_pad_left = 2`)
    const FOOTER_PAD_LEFT: u16 = 2;
    let _ = theme;
    let inner = Rect {
        x: area.x.saturating_add(FOOTER_PAD_LEFT),
        y: area.y,
        width: area.width.saturating_sub(FOOTER_PAD_LEFT),
        height: area.height,
    };

    // App-level double-press confirmation (quit via Ctrl+Q / Ctrl+C / Ctrl+D) takes precedence over the dashboard-local stop-confirm
    // Both render through `with_pending`
    // Otherwise the keys would set a pending quit but the dashboard would show no "press again" feedback
    if let Some(pending) = pending_hint {
        ShortcutsBar::new(&[])
            .with_pending(Some(pending))
            .render(inner, buf);
        return;
    }

    // A live delete-confirm owns the footer: `y`/`n` when the list is focused, else the second-`Ctrl+X` "press again" hint
    // An expired arm falls through to the normal hints
    if state.armed_delete_row_ref().is_some() {
        if state.list_focused {
            let confirm_label =
                if matches!(state.selected_stop_action, Some(DashboardStopAction::Close)) {
                    "confirm close"
                } else if state.workspace_membership_mode {
                    "confirm archive"
                } else {
                    "confirm delete"
                };
            let hints = vec![
                HintItem::new(key!('y'), confirm_label),
                HintItem::new(key!('n'), "cancel"),
            ];
            ShortcutsBar::new(&hints)
                .compact(4, None)
                .render(inner, buf);
        } else {
            let stop_key = registry
                .find(crate::actions::ActionId::DashboardStop)
                .map(|d| d.default_key)
                .unwrap_or_else(|| key!('x', CONTROL));
            let pending = PendingHint {
                shortcut: stop_key,
                label: if let Some(action) = state.selected_stop_action {
                    action.confirmation_label().unwrap_or("stop this session")
                } else if state.workspace_membership_mode {
                    "archive this session"
                } else {
                    "delete this session"
                },
            };
            ShortcutsBar::new(&[])
                .with_pending(Some(pending))
                .render(inner, buf);
        }
        return;
    }

    // An active rename owns the keyboard (`handle_key` routes to `handle_rename_key` before anything else)
    // The footer therefore shows exactly its two actions instead of the dispatch and nav hints
    if state.rename.is_some() {
        let hints = vec![
            HintItem::new(key!(Enter), "save"),
            HintItem::new(key!(Esc), "cancel"),
        ];
        ShortcutsBar::new(&hints)
            .compact(4, None)
            .render(inner, buf);
        return;
    }

    // Search mode owns the footer: show how to confirm or cancel the live filter rather than the dispatch and nav hints
    if state.search_mode {
        let hints = vec![
            HintItem::paired(key!(Up), key!(Down), "nav"),
            HintItem::new(key!(Enter), "apply"),
            HintItem::new(key!(Esc), "cancel"),
        ];
        ShortcutsBar::new(&hints)
            .compact(4, None)
            .render(inner, buf);
        return;
    }

    let show_ctrl_x = state
        .selected
        .as_ref()
        .is_none_or(|row| !row.is_workspace() || state.workspace_membership_mode)
        && selected_state.is_some_and(|s| {
            matches!(s, RowState::Working | RowState::NeedsInput) || s.allows_delete()
        });
    let stop_label = if state.workspace_membership_mode {
        state
            .selected_stop_action
            .map_or("stop", |action| action.label())
    } else if matches!(
        selected_state,
        Some(RowState::Working | RowState::NeedsInput)
    ) {
        "stop"
    } else {
        "delete"
    };

    // Overview list focused (via Tab), navigation hints: arrows / j-k move between agents, Enter opens
    // the focused one, Tab returns to the input.
    if state.list_focused && !peek_active {
        let key_for = |id: crate::actions::ActionId, fallback: KeyShortcut| -> KeyShortcut {
            registry.find(id).map(|d| d.default_key).unwrap_or(fallback)
        };
        let stop = key_for(crate::actions::ActionId::DashboardStop, key!('x', CONTROL));
        let help = key_for(
            crate::actions::ActionId::DashboardShortcutsHelp,
            key!('.', CONTROL),
        );
        // The ↑/↓ (and vim j/k) nav chip is intentionally omitted.
        if state.selected_idle_overflow {
            let toggle = if state.idle_show_all {
                "show fewer"
            } else {
                "show all"
            };
            let hints = vec![
                HintItem::new(key!(Enter), toggle),
                HintItem::new(key!(Tab), "input"),
            ];
            ShortcutsBar::new(&hints)
                .compact(4, Some(HintItem::new(help, "shortcuts")))
                .render(inner, buf);
            return;
        }
        // Section header under the cursor: Enter toggles the section, not a row, so `open` / `stop` would lie
        // Tab hands focus back to the dispatch input (Esc does too, one tier at a time)
        if let Some(section) = state.selected_section {
            let toggle = if state.is_section_collapsed(section) {
                "expand"
            } else {
                "collapse"
            };
            let hints = vec![
                HintItem::new(key!(Enter), toggle),
                HintItem::new(key!(Tab), "input"),
            ];
            ShortcutsBar::new(&hints)
                .compact(4, Some(HintItem::new(help, "shortcuts")))
                .render(inner, buf);
            return;
        }
        // Enter acts on the focused actions-row item; the arrows aren't advertised, the row reads as a row
        if let Some(enter_label) = state.focused_action_label() {
            let mut hints = vec![HintItem::new(key!(Enter), enter_label)];
            // A draft on `+ New Agent` sends from the list pane too, so offer the same send+open chord the input pane shows
            if state.focused_new_agent_sends_draft() {
                hints.push(HintItem::new(key!('s', CONTROL), "send+open"));
            }
            hints.push(HintItem::new(key!(Tab), "input"));
            ShortcutsBar::new(&hints)
                .compact(4, Some(HintItem::new(help, "shortcuts")))
                .render(inner, buf);
            return;
        }
        let mut hints = vec![
            HintItem::new(key!(Enter), "open"),
            HintItem::new(key!(Tab), "input"),
        ];
        if show_ctrl_x {
            hints.push(HintItem::new(stop, stop_label).pinned());
        }

        ShortcutsBar::new(&hints)
            .compact(4, Some(HintItem::new(help, "shortcuts")))
            .render(inner, buf);
        return;
    }

    let resolve = |id: crate::actions::ActionId, fallback: KeyShortcut| -> KeyShortcut {
        registry.find(id).map(|d| d.default_key).unwrap_or(fallback)
    };
    let enter = key!(Enter);
    // "Send + open" is `Ctrl+S`; `Shift+Enter` inserts a newline instead
    // It is hardcoded in the dispatch / peek key handlers, not a registry action, so the chip is built directly
    let send_open = key!('s', CONTROL);
    // Multiline: bare Enter inserts a newline; Shift+Enter (or Alt+Enter over SSH / when Shift+Enter collapses) sends
    // This matches the agent prompt keybar
    let send_key = if state.multiline_mode {
        if crate::terminal::terminal_context().prefer_alt_enter_newline() {
            key!(Enter, ALT)
        } else {
            key!(Enter, SHIFT)
        }
    } else {
        enter
    };
    let stop = resolve(crate::actions::ActionId::DashboardStop, key!('x', CONTROL));
    let help = resolve(
        crate::actions::ActionId::DashboardShortcutsHelp,
        key!('.', CONTROL),
    );

    let help_hint = HintItem::new(help, "shortcuts");

    // Submit chord is `send_key` (Enter, or Shift/Alt+Enter in multiline). Ctrl+S is send+open.
    // Empty draft: create/open on the submit chord; non-empty: send
    let row_selected = state.selected.is_some();
    let prompt_empty = state.dispatch.text().trim().is_empty();

    let hints: Vec<HintItem> = if peek_active {
        // Peek mode: Enter labels mirror `DashboardState::handle_peek_key`: vim + unfocused reply → Enter
        // focuses reply ("input") focused + non-empty → Enter sends otherwise → Enter opens / attaches.
        let esc = key!(Esc);
        // Pending permission / ask-tool: `1-9` selects even while unfocused (the handler focuses the panel)
        // When focused with a selected option, Enter answers
        // Mirrors `DashboardState::handle_peek_key`.
        let has_pending_question = state
            .peek
            .as_ref()
            .is_some_and(|p| p.question.is_some() && !p.options.is_empty());
        let option_selected = state
            .peek
            .as_ref()
            .is_some_and(|p| p.selected_option.is_some());
        let reply_empty = state.peek_reply.text().trim().is_empty();
        let esc_label = if reply_empty { "New Agent" } else { "back" };
        // Pin Esc when it clears a draft (`back`) so compact doesn't drop it behind stop/help; that matches its importance in handle_peek_key
        let esc_hint = {
            let h = HintItem::new(esc, esc_label);
            if reply_empty { h } else { h.pinned() }
        };
        let vim_mode = crate::appearance::cache::load_vim_mode();
        // Two-focus model: Tab toggles between the reply and row nav. Vim opens the reply unfocused so j/k keep selecting.
        let peek_focused = state.peek.as_ref().map(|p| p.focused).unwrap_or(true);
        let question_focused = peek_focused && has_pending_question;
        let tab_hint = HintItem::new(key!(Tab), if peek_focused { "list" } else { "input" });
        // `1-9 select` hint for the question picker (no single bound key).
        let select_hint = HintItem {
            keys: vec![],
            label: "select".into(),
            custom_display: Some("1-9"),
            description: None,
            pinned: false,
        };
        if question_focused && option_selected {
            // An option is selected, so Enter answers
            // `Tab` unfocuses to the row list (the same two-focus toggle the other peek states show)
            // ↑/↓ still move within the options; the nav chip is dropped to save bottom-bar space
            vec![HintItem::new(enter, "answer"), tab_hint, esc_hint]
        } else if has_pending_question && peek_focused {
            // Question pending, focused, nothing selected: navigation and select
            let mut h = vec![
                HintItem::new(enter, "open"),
                select_hint,
                tab_hint,
                esc_hint,
            ];
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        } else if vim_mode && !peek_focused {
            // Vim unfocused: Enter focuses the reply (not open/send).
            // Right still attaches; show it so open stays discoverable
            // Pending question: keep 1-9 select (digits still work unfocused).
            let mut h = vec![
                HintItem::new(enter, "input"),
                // Pin open: attach is the replacement for Enter in this mode.
                HintItem::new(key!(Right), "open").pinned(),
                tab_hint,
                esc_hint,
            ];
            if has_pending_question {
                h.insert(2, select_hint);
            }
            if !reply_empty {
                h.insert(1, HintItem::new(send_open, "send+open"));
            }
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        } else if has_pending_question {
            // Non-vim unfocused (or other) with a pending question: open and select
            let mut h = vec![
                HintItem::new(enter, "open"),
                select_hint,
                tab_hint,
                esc_hint,
            ];
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        } else if peek_focused && !reply_empty {
            vec![
                HintItem::new(send_key, "send"),
                HintItem::new(send_open, "send+open"),
                tab_hint,
                HintItem::new(esc, "back").pinned(),
            ]
        } else {
            // Focused empty: open is on the submit chord (send_key)
            // Unfocused: bare Enter still attaches
            let open_key = if peek_focused { send_key } else { enter };
            let mut h = vec![HintItem::new(open_key, "open"), tab_hint, esc_hint];
            if show_ctrl_x {
                h.push(HintItem::new(stop, stop_label).pinned());
            }
            h
        }
    } else if let Some(section) = state.selected_section {
        // A section header is selected. No stop chip in either state; there's no session under a section header.
        if prompt_empty {
            // ↑↓ navigate, Enter toggles collapse/expand, Esc returns to the `+ New Agent` button
            let toggle = if state.is_section_collapsed(section) {
                "expand"
            } else {
                "collapse"
            };
            vec![
                HintItem::new(enter, toggle),
                HintItem::new(key!(Esc), "New Agent"),
            ]
        } else {
            // Typed text dispatches a NEW agent (a section header is never a reply target)
            // Show the same chips as the `+ New Agent` button with a draft
            // send_key sends (stays on the dashboard), Ctrl+S sends and opens detail, Shift+Tab cycles the dispatch mode
            vec![
                HintItem::new(send_key, "send"),
                HintItem::new(send_open, "send+open"),
                HintItem::new(key!(BackTab), "mode"),
            ]
        }
    } else if state.selected_idle_overflow {
        // The Idle overflow toggle is selected. Like a section header, there's no session under it, so no stop chip.
        if prompt_empty {
            let toggle = if state.idle_show_all {
                "show fewer"
            } else {
                "show all"
            };
            vec![
                HintItem::new(enter, toggle),
                HintItem::new(key!(Esc), "New Agent"),
            ]
        } else {
            vec![
                HintItem::new(send_key, "send"),
                HintItem::new(send_open, "send+open"),
                HintItem::new(key!(BackTab), "mode"),
            ]
        }
    } else if let Some(enter_label) = state.focused_action_label() {
        let mut h: Vec<HintItem> = vec![];
        if prompt_empty {
            // Same label as the list-focused footer: an empty Enter acts on the focused item from either pane
            h.push(HintItem::new(send_key, enter_label));
            h.push(HintItem::new(key!(Tab), "list"));
        } else {
            h.push(HintItem::new(send_key, "send"));
            h.push(HintItem::new(send_open, "send+open"));
        }
        h.push(HintItem::new(key!(BackTab), "mode"));
        h
    } else if row_selected {
        let mut h: Vec<HintItem> = vec![];
        if prompt_empty {
            h.push(HintItem::new(send_key, "open"));
            h.push(HintItem::new(key!(Tab), "list"));
        } else {
            h.push(HintItem::new(send_key, "send"));
            h.push(HintItem::new(send_open, "send+open"));
        }
        if show_ctrl_x {
            h.push(HintItem::new(stop, stop_label).pinned());
        }
        h
    } else {
        // Defensive: neither the button nor a row is focused
        vec![HintItem::new(send_key, "create")]
    };

    ShortcutsBar::new(&hints)
        .compact(4, Some(help_hint))
        .render(inner, buf);
}

fn state_icon(state: RowState, tick: u64) -> &'static str {
    match state {
        RowState::Working => {
            let frames = crate::glyphs::dot_spinner_frames();
            let i = (tick / SPINNER_DIVISOR) as usize % frames.len();
            frames.get(i).copied().unwrap_or("")
        }
        // Hollow diamond for idle rows; filled diamond for every state that needs visual presence (needs-input, completed, failed)
        // The foreground colour disambiguates (accent_user for needs-input, accent_success for done, accent_error for failed)
        RowState::Idle | RowState::Inactive => crate::glyphs::diamond_hollow(),
        RowState::NeedsInput | RowState::Completed | RowState::Failed => {
            crate::glyphs::diamond_filled()
        }
    }
}

fn state_color(state: RowState, theme: &Theme) -> Color {
    match state {
        RowState::Working => theme.accent_running,
        RowState::NeedsInput => theme.warning,
        RowState::Idle | RowState::Inactive => theme.gray_dim,
        RowState::Completed => theme.accent_success,
        RowState::Failed => theme.accent_error,
    }
}

fn needs_input_bullet_color(tick: u64, theme: &Theme) -> Color {
    let bright = (tick / NEEDS_INPUT_BLINK_DIVISOR).is_multiple_of(2);
    if bright {
        theme.warning
    } else {
        needs_input_dim_color(theme).unwrap_or(theme.warning)
    }
}

fn needs_input_dim_color(theme: &Theme) -> Option<Color> {
    crate::render::color::blend_color(theme.bg_base, theme.warning, 0.5)
}

fn needs_input_blink_visible(theme: &Theme) -> bool {
    needs_input_dim_color(theme).is_some_and(|dim| dim != theme.warning)
}

/// Process-wide cached home directory via [`distill_dirs::home_dir`].
/// Shared by render and dispatch (`dispatch_dashboard_select`) so we don't re-resolve on every keystroke.
pub(crate) fn cached_home() -> Option<&'static str> {
    HOME.get_or_init(|| {
        distill_dirs::home_dir()
            .map(|home| home.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
    })
    .as_deref()
}

static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Only a dynamic top inset is reserved for the dashboard banner.
pub fn popup_rect(view: Rect) -> Rect {
    // Popup takes the FULL bottom area (no horizontal inset, no bottom inset). Only a small TOP inset
    // is reserved for the dashboard banner that shows the live rows. A BANNER_MIN_HEIGHT floor applies
    // on tall terminals so a 1-row banner doesn't crowd the rows out.
    const BANNER_MIN_HEIGHT: u16 = 6;
    const BANNER_MAX_HEIGHT: u16 = 14;

    let banner_h: u16 = if view.height >= BANNER_MIN_HEIGHT + 10 {
        (view.height / 3).clamp(BANNER_MIN_HEIGHT, BANNER_MAX_HEIGHT)
    } else {
        0
    };
    Rect {
        x: view.x,
        y: view.y + banner_h,
        width: view.width,
        height: view.height.saturating_sub(banner_h),
    }
}

/// `title_label` is passed by the caller rather than computed here from a borrowed `AgentView`. The
/// closure can then take a mutable borrow of the agents map without conflicting with the title
/// lookup.
pub fn render_popup_overlay(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    title_label: &str,
    state: &mut DashboardState,
    draw_agent: impl FnOnce(
        Rect,
        &mut Buffer,
    ) -> (
        Option<(u16, u16)>,
        Option<crate::terminal::overlay::PostFlush>,
    ),
) -> (
    Option<(u16, u16)>,
    Option<crate::terminal::overlay::PostFlush>,
    bool,
) {
    use ratatui::widgets::{Block, Borders, Clear, Widget};

    if area.area() == 0 {
        state.popup_close_rect = None;
        state.popup_outer_rect = None;
        return (None, None, false);
    }
    state.popup_outer_rect = Some(area);

    let border_color = theme.selection_border;

    // The canonical bordered-frame primitive, used by `AgentView::draw_subagent_fullscreen` in app/agent_view/subagent_takeover.rs
    // Paints the header (top border and title row), the divider (T-junctions), and the content frame with full borders
    // The divider sits ABOVE the returned `content` rect, so `draw_agent` cannot overwrite it
    let Some(frame) =
        crate::views::picker::render_bordered_frame(buf, area, border_color, theme.bg_base)
    else {
        // Too small for the canonical frame (height < 5 or width < 10). The agent is never drawn on this
        // branch (we return `(None, None)` below without invoking `draw_agent`). Its hit-area maps
        // therefore stay empty by design. The user's only exit is. EscEsc / Ctrl+\\ / a wider terminal.
        Clear.render(area, buf);
        buf.set_style(area, Style::default().bg(theme.bg_base));
        let outline = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color).bg(theme.bg_base));
        outline.render(area, buf);
        if area.height >= 3 && area.width >= 6 {
            let hint = truncate_str(
                "(terminal too small: Esc to close)",
                area.width.saturating_sub(2) as usize,
            );
            buf.set_string(
                area.x + 1,
                area.y + area.height / 2,
                hint,
                theme.dim().bg(theme.bg_base),
            );
        }
        state.popup_close_rect = None;
        return (None, None, false);
    };

    let title_row = frame.title_row;
    let inner = frame.content;

    let title_text = format!(" \u{2771} {title_label} ");

    let close_label = crate::glyphs::ballot_x_button();
    let close_w = UnicodeWidthStr::width(close_label) as u16;
    // Reserve the close button's width plus a 1-cell gap on the right; `truncate_str` handles overflow with an ellipsis
    let title_max = title_row.width.saturating_sub(close_w + 2).max(1) as usize;
    let truncated = truncate_str(&title_text, title_max);
    buf.set_string(
        title_row.x,
        title_row.y,
        truncated,
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    );

    // Close button: register its hit rect so `handle_mouse` can dispatch a popup close on click
    if close_w + 1 < title_row.width {
        let close_x = title_row.x + title_row.width - close_w;
        buf.set_string(
            close_x,
            title_row.y,
            close_label,
            Style::default().fg(theme.gray).bg(theme.bg_base),
        );
        state.popup_close_rect = Some(Rect {
            x: close_x,
            y: title_row.y,
            width: close_w,
            height: 1,
        });
    } else {
        state.popup_close_rect = None;
    }

    let (cursor, post_flush) = draw_agent(inner, buf);
    (cursor, post_flush, true)
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
