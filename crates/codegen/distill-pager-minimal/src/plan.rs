// Modified for Distill by Samuel Fajreldines, 2026.
//! The full TUI renders plan approval as a fullscreen line-viewer plus a live feedback prompt.
//! Minimal instead commits the whole plan into native scrollback as a normal conversation block (see [`maybe_commit_plan`]).
//! The plan then reads and scrolls exactly like the rest of the transcript.
//! The prompt-anchored live region holds only the decision controls (approve / revise / keep planning) plus the feedback input when revising.
//! Nothing of the plan body is drawn under the prompt.
//!
//! Input routing is unchanged: while `line_viewer.is_some()` the agent's input handler already routes keys to the two plan handlers.
//! `handle_line_viewer_key` owns Preview focus (`a` approve / `s`/`Tab` revise / `q` keep planning).
//! `handle_plan_feedback_key` owns Prompt focus (type feedback, `Enter` send, `Esc` back).
//! Minimal keeps the line viewer open (so those keys fire) but renders this compact controls strip in place of the never-drawn fullscreen viewer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use distill_pager::app::agent_view::AgentView;
use distill_pager::app::app_view::{ActiveView, AppView};
use distill_pager::minimal_api;
use distill_pager::scrollback::block::RenderBlock;
use distill_pager::theme::Theme;
use distill_pager::views::plan_approval_view::PlanApprovalFocus;
use distill_pager::views::prompt_widget::{PromptBg, PromptStyle};

/// The active plan-approval focus, defaulting to `Preview`.
fn focus(agent: &AgentView) -> PlanApprovalFocus {
    minimal_api::plan_approval_view(agent)
        .map(|p| p.focus)
        .unwrap_or(PlanApprovalFocus::Preview)
}

/// Scrollback notice when exit_plan_mode parks with no plan body.
/// Kept short and plain (no markdown chrome) so native scrollback reads cleanly under minimal mode's chromeless commit path.
const EMPTY_PLAN_SCROLLBACK: &str = "\
No plan written yet.

Approve to leave plan mode and start implementing, request changes to send the \
agent back to planning, or quit to abandon.";

/// Controls-strip header for a parked plan approval.
fn plan_header(has_plan: bool) -> &'static str {
    if has_plan {
        "Plan ready for review"
    } else {
        "No plan written yet"
    }
}

/// Body committed into native scrollback for a parked plan approval.
fn plan_scrollback_body(plan_content: Option<&str>) -> String {
    plan_content
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| EMPTY_PLAN_SCROLLBACK.to_owned())
}

/// Commit each plan (and revision) once, anchored above the still-running `exit_plan_mode` row so the clipped live tail cannot hide its head.
/// Empty plans still commit a notice; otherwise only the controls strip shows and the session looks stuck.
/// Deliberate render-path push into `ScrollbackState`: client state, not a server event, so a resumed session will not replay it.
pub fn maybe_commit_plan(app: &mut AppView) {
    let ActiveView::Agent(id) = &app.active_view else {
        return;
    };
    let id = *id;

    // Extract the plan (owned) under a short immutable borrow
    // The mutable scrollback push and the `minimal_state` read/write below must not overlap it
    let plan = app.agents.get(&id).and_then(|agent| {
        minimal_api::plan_approval_view(agent).map(|pav| {
            let content = plan_scrollback_body(pav.plan_content.as_deref());
            (pav.tool_call_id.clone(), content)
        })
    });
    let Some((tool_call_id, content)) = plan else {
        return;
    };

    if minimal_api::minimal_committed_plan_id(app) == Some(tool_call_id.as_str()) {
        return; // already emitted this plan
    }

    // Mark the plan as emitted only when the block was actually pushed
    // The agent borrow can't fail here (the plan was just extracted from it)
    // If it ever did, stamping the id anyway would treat the plan as committed while nothing ever reaches native scrollback
    if let Some(agent) = app.agents.get_mut(&id) {
        let block = RenderBlock::agent_message(content);
        // No anchor (the tool was reaped): append, and the plan commits at turn end
        match minimal_api::pending_tool_entry_id(agent, &tool_call_id) {
            Some(anchor) => {
                agent.scrollback.insert_block_before(anchor, block);
            }
            None => {
                agent.scrollback.push_block(block);
            }
        }
        minimal_api::set_minimal_committed_plan_id(app, Some(tool_call_id));
    }
}

/// Desired controls-strip height: header + controls + optional feedback input.
pub fn height(agent: &AgentView) -> u16 {
    let input = if focus(agent) == PlanApprovalFocus::Prompt {
        1
    } else {
        0
    };
    // header (1) + controls (1) + input (0/1)
    2u16.saturating_add(input)
}

/// Render the compact plan-approval controls strip into `area`. This only draws the header, the decision hint, and
/// (when revising) the feedback input.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    agent: &mut AgentView,
    theme: &Theme,
) -> Option<(u16, u16)> {
    if area.height == 0 || area.width < 4 {
        return None;
    }
    let foc = focus(agent);
    let input_h: u16 = if foc == PlanApprovalFocus::Prompt {
        1
    } else {
        0
    };

    // header (1) · controls (1) · input (0/1)
    let controls_y = (area.y + area.height).saturating_sub(1 + input_h);

    // ── header ──
    let has_plan = minimal_api::plan_approval_view(agent)
        .map(|p| p.has_plan)
        .unwrap_or(false);
    let header_style = Style::default()
        .fg(theme.accent_user)
        .bg(Color::Reset)
        .add_modifier(Modifier::BOLD);
    buf.set_style(
        Rect { height: 1, ..area },
        Style::default().bg(Color::Reset),
    );
    buf.set_span(
        area.x,
        area.y,
        &Span::styled(plan_header(has_plan), header_style),
        area.width,
    );

    // ── controls hint ──
    let has_content = minimal_api::plan_approval_view(agent)
        .map(|p| !p.comments.is_empty())
        .unwrap_or(false)
        || !agent.prompt.text().trim().is_empty();
    // Tab reopens the preview (including the empty-plan placeholder).
    let hint = match foc {
        PlanApprovalFocus::Prompt if has_content => {
            "enter request changes \u{00b7} tab plan \u{00b7} esc back"
        }
        PlanApprovalFocus::Prompt => "enter approve \u{00b7} tab plan \u{00b7} esc back",
        PlanApprovalFocus::Commenting => "enter save comment \u{00b7} esc cancel",
        PlanApprovalFocus::Preview => "a approve \u{00b7} s revise \u{00b7} q keep planning",
    };
    let hint_style = theme.dim().bg(Color::Reset);
    let controls_rect = Rect {
        x: area.x,
        y: controls_y,
        width: area.width,
        height: 1,
    };
    buf.set_style(controls_rect, hint_style);
    buf.set_span(
        area.x,
        controls_y,
        &Span::styled(hint, hint_style),
        area.width,
    );

    // ── feedback input (revise mode) ──
    if input_h > 0 {
        let row = Rect {
            x: area.x,
            y: (area.y + area.height).saturating_sub(1),
            width: area.width,
            height: 1,
        };
        let style = input_style(theme);
        buf.set_style(row, Style::default().bg(theme.bg_visual));
        return agent
            .prompt
            .draw(buf, row, None, &style, None, None)
            .cursor_pos;
    }
    None
}

/// Chromeless prompt style for the feedback editor (the modal supplies framing).
fn input_style(theme: &Theme) -> PromptStyle {
    PromptStyle {
        focused: true,
        show_prefix: false,
        vpad_top: 0,
        compact: false,
        chrome: false,
        chrome_pad_left: 0,
        chrome_pad_right: 0,
        bg: PromptBg::Panel(theme.bg_visual),
        accent_color_override: None,
        border_color_override: None,
        prefix_override: None,
        placeholder_when_focused: false,
        placeholder_override: None,
        show_accent_line: false,
        show_borders: false,
        title: None,
        image_preview: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_plan_scrollback_uses_notice_not_silence() {
        let body = plan_scrollback_body(None);
        assert!(body.contains("No plan written yet"));
        assert!(body.contains("Approve"));

        let whitespace = plan_scrollback_body(Some("  \n\t  "));
        assert_eq!(whitespace, body, "whitespace-only counts as empty");

        let real = plan_scrollback_body(Some("# Plan\n- do it"));
        assert_eq!(real, "# Plan\n- do it");
    }
}
