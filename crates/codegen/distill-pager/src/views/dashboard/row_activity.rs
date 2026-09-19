//! Activity and secondary-line presentation for local dashboard rows.

use crate::app::agent_view::AgentView;
use crate::app::subagent::format_activity_label;
use crate::views::dashboard::row::{RowBadge, sanitize};
use crate::views::dashboard::state::RowState;

/// Parent turn, wake, command, pending dispatch, or replay activity.
/// Background work alone can keep the row Working without making the parent's activity live.
pub(crate) fn has_live_parent_activity(agent: &AgentView) -> bool {
    !agent.session.state.is_idle()
        || agent.wake_turn_active()
        || agent.session.turn_activity().is_some()
        || !agent.session.pending_prompts.is_empty()
        || agent.session.loading_replay
}

/// Pending input takes precedence over live activity and the last-turn preview.
pub(crate) fn top_level_secondary_line(
    agent: &AgentView,
    state: RowState,
    activity: Option<&str>,
) -> Option<String> {
    match state {
        RowState::NeedsInput => {
            if let Some(perm) = agent.permission_queue.front() {
                let title = perm.title.trim();
                if !title.is_empty() {
                    return Some(format!("Pending: {}", sanitize(title)));
                }
            }
            if agent.question_view.is_some() {
                return Some("Pending: question".to_string());
            }
            activity.map(sanitize)
        }
        RowState::Working if has_live_parent_activity(agent) => activity.map(sanitize),
        RowState::Working
        | RowState::Idle
        | RowState::Inactive
        | RowState::Completed
        | RowState::Failed => {
            // The local model already appears in peek; it is not a last-turn preview.
            agent
                .last_turn_summary
                .as_deref()
                .map(sanitize)
                .or_else(|| last_agent_message_preview(agent))
        }
    }
}

/// Walk the scrollback from the end, returning the first `AgentMessage` block's text trimmed to a single line.
/// Returns `None` when no agent message has been produced yet.
fn last_agent_message_preview(agent: &AgentView) -> Option<String> {
    use crate::scrollback::block::RenderBlock;
    let len = agent.scrollback.len();
    for idx in (0..len).rev() {
        let entry = agent.scrollback.get(idx)?;
        if let RenderBlock::AgentMessage(msg) = &entry.block {
            let text = msg.text();
            let line = first_nonempty_line(&text)?;
            return Some(sanitize(line.trim()));
        }
    }
    None
}

/// Return the first line containing non-whitespace text, preserving its surrounding whitespace.
fn first_nonempty_line(s: &str) -> Option<&str> {
    for line in s.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(line);
        }
    }
    None
}

pub(crate) fn top_level_activity(agent: &AgentView, state: RowState) -> Option<String> {
    match state {
        RowState::NeedsInput => Some("Awaiting your input".to_owned()),
        RowState::Working if has_live_parent_activity(agent) => {
            if let Some(cmd) = agent.session.state.command_in_flight() {
                Some(format!("{}…", cmd.display_name()))
            } else if let Some(activity) = agent.resolve_turn_activity() {
                Some(sanitize(&format_activity_label(&activity)))
            } else if agent.session.loading_replay {
                Some("Loading…".to_string())
            } else {
                Some("Working".to_string())
            }
        }
        RowState::Working
        | RowState::Idle
        | RowState::Inactive
        | RowState::Completed
        | RowState::Failed => None,
    }
}

pub(crate) fn live_work_badges(agent: &AgentView) -> impl Iterator<Item = RowBadge> {
    let counts = agent.watchers();
    [
        (counts.subagents, RowBadge::Subagents(counts.subagents)),
        (counts.commands, RowBadge::Tasks(counts.commands)),
        (
            counts.monitors + counts.loops,
            RowBadge::Watchers(counts.monitors + counts.loops),
        ),
        (counts.workflows, RowBadge::Workflows(counts.workflows)),
    ]
    .into_iter()
    .filter_map(|(count, badge)| (count > 0).then_some(badge))
}

#[cfg(test)]
#[path = "row_activity_tests.rs"]
mod tests;
