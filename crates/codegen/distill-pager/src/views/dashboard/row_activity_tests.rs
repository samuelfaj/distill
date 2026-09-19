use std::time::{Duration, Instant};

use indexmap::IndexMap;
use pretty_assertions::assert_eq;

use super::*;
use crate::app::agent::{AgentCommand, AgentId, AgentState, BgTaskStatus};
use crate::app::agent_test_fixtures::{running_bg_task, scheduled_loop};
use crate::app::agent_view::test_fixtures::{
    add_running_execute, make_followup_permission_state, running_subagent_info,
};
use crate::scrollback::block::RenderBlock;
use crate::views::dashboard::row::{DashboardRow, build_rows};
use crate::views::dashboard::state::{Filter, Grouping};
use crate::views::workflows::WorkflowRunSnapshot;

fn agent() -> AgentView {
    let mut agent = crate::test_util::make_agent_view(Some("session"), "/tmp");
    agent.display_name = Some("Dashboard regression".to_owned());
    agent.last_turn_summary = Some("Summary wins".to_owned());
    agent.scrollback.push_block(RenderBlock::agent_message(
        "\n  Assistant preview\nsecond line",
    ));
    agent
}

fn row(agent: AgentView) -> DashboardRow {
    build_rows(
        &IndexMap::from([(AgentId(0), agent)]),
        &Default::default(),
        &[],
        Grouping::State,
        &Filter::None,
        None,
    )
    .remove(0)
}

#[test]
fn idle_background_task_shows_last_turn_summary() {
    let mut agent = agent();
    agent
        .session
        .bg_tasks
        .insert("task".to_owned(), running_bg_task("task", false));
    let row = row(agent);
    assert_eq!(
        (
            RowState::Working,
            None,
            Some("Summary wins"),
            vec![RowBadge::Tasks(1)]
        ),
        (
            row.state,
            row.activity,
            row.secondary_line.as_deref(),
            row.badges
        )
    );
    assert!(!row.state.allows_delete());
}

#[test]
fn idle_monitor_and_loop_show_summary_and_watcher_chip() {
    for (monitor, scheduled) in [(true, false), (false, true), (true, true)] {
        let mut agent = agent();
        if monitor {
            agent
                .session
                .bg_tasks
                .insert("monitor".to_owned(), running_bg_task("monitor", true));
        }
        if scheduled {
            agent
                .session
                .scheduled_tasks
                .insert("loop".to_owned(), scheduled_loop("loop"));
        }
        let row = row(agent);
        assert_eq!(
            (
                RowState::Working,
                Some("Summary wins"),
                vec![RowBadge::Watchers(
                    usize::from(monitor) + usize::from(scheduled)
                )]
            ),
            (row.state, row.secondary_line.as_deref(), row.badges)
        );
    }
}

#[test]
fn idle_background_without_summary_uses_assistant_preview_or_no_line() {
    let mut agent = agent();
    agent.last_turn_summary = None;
    agent
        .session
        .bg_tasks
        .insert("task".to_owned(), running_bg_task("task", false));
    assert_eq!(
        Some("Assistant preview"),
        top_level_secondary_line(&agent, RowState::Working, None).as_deref()
    );
    agent.scrollback = crate::scrollback::state::ScrollbackState::new();
    assert_eq!(None, row(agent).secondary_line);
}

#[test]
fn live_turn_wake_command_loading_and_dispatch_keep_activity() {
    for case in 0..6 {
        let mut agent = agent();
        agent
            .session
            .bg_tasks
            .insert("task".to_owned(), running_bg_task("task", false));
        let expected = match case {
            0 => {
                agent.session.state = AgentState::TurnRunning;
                "Waiting for response…"
            }
            1 => {
                agent.note_streaming_wake_turn("wake");
                "Working"
            }
            2 => {
                agent.session.state = AgentState::CommandRunning {
                    command: AgentCommand::Compact,
                    started_at: Instant::now(),
                };
                "Compacting…"
            }
            3 => {
                agent.session.loading_replay = true;
                "Loading…"
            }
            4 => {
                agent.session.enqueue_prompt("dispatch".to_owned());
                "Working"
            }
            _ => {
                add_running_execute(&mut agent);
                "Running: sleep 5"
            }
        };
        let row = row(agent);
        assert_eq!(
            (RowState::Working, Some(expected), vec![RowBadge::Tasks(1)]),
            (row.state, row.secondary_line.as_deref(), row.badges),
            "case {case}"
        );
    }
}

#[test]
fn needs_input_keeps_pending_preview_with_live_chips() {
    let mut agent = agent();
    agent
        .session
        .bg_tasks
        .insert("task".to_owned(), running_bg_task("task", false));
    let mut permission = make_followup_permission_state();
    permission.title = "  Run approval  ".to_owned();
    agent.permission_queue.push_back(permission);
    let row = row(agent);
    assert_eq!(
        (
            RowState::NeedsInput,
            Some("Pending: Run approval"),
            vec![RowBadge::NeedsInput, RowBadge::Tasks(1)]
        ),
        (row.state, row.secondary_line.as_deref(), row.badges)
    );
}

fn workflow(status: &str) -> WorkflowRunSnapshot {
    WorkflowRunSnapshot {
        run_id: status.to_owned(),
        name: "flow".to_owned(),
        objective: String::new(),
        status: status.to_owned(),
        management_available: true,
        builtin: false,
        phases: Vec::new(),
        current_phase: None,
        agents: Vec::new(),
        agent_budget: None,
        agents_used: 0,
        agents_reserved: 0,
        agents_remaining: None,
        agent_usage_incomplete: false,
        active_agents: 0,
        elapsed_ms: 0,
        received_at: Instant::now(),
        pause_message: None,
        result_summary: None,
    }
}

#[test]
fn live_chips_count_all_buckets_and_exclude_finished_workflow_children_and_queue() {
    let mut agent = agent();
    for (id, background, finished, owned) in [
        ("fg", false, false, false),
        ("bg", true, false, false),
        ("done", true, true, false),
        ("owned", true, false, true),
    ] {
        let mut child = running_subagent_info(id);
        child.attempt.is_background = background;
        child.set_finished_for_test(finished);
        child.attempt.workflow_run_id = owned.then(|| "flow".into());
        agent.subagent_sessions.insert(id.to_owned(), child);
    }
    for (id, monitor, status) in [
        ("task", false, BgTaskStatus::Running),
        ("monitor", true, BgTaskStatus::Running),
        ("done", false, BgTaskStatus::Done),
        ("failed", true, BgTaskStatus::Failed),
    ] {
        let mut task = running_bg_task(id, monitor);
        task.status = status;
        agent.session.bg_tasks.insert(id.to_owned(), task);
    }
    agent
        .session
        .scheduled_tasks
        .insert("loop".to_owned(), scheduled_loop("loop"));
    agent.workflow_runs = vec![
        workflow("active"),
        workflow("complete"),
        workflow("user_paused"),
    ];
    agent.session.enqueue_prompt("queued".to_owned());
    agent
        .shared_queue
        .push(crate::app::prompt_queue::QueueEntryWire {
            id: "held".to_owned(),
            version: 1,
            owner: None,
            last_editor: None,
            kind: "prompt".to_owned(),
            text: "held".to_owned(),
            combined_texts: None,
            position: 0,
        });
    assert_eq!(
        vec![
            RowBadge::Subagents(2),
            RowBadge::Tasks(1),
            RowBadge::Watchers(2),
            RowBadge::Workflows(1)
        ],
        row(agent).badges
    );
}

#[test]
fn live_chips_update_on_completion_without_changing_row_classification() {
    let mut agent = agent();
    let anchor = Instant::now() - Duration::from_secs(60);
    agent.last_active_at = Some(anchor);
    agent
        .session
        .bg_tasks
        .insert("task".to_owned(), running_bg_task("task", false));
    agent
        .session
        .scheduled_tasks
        .insert("loop".to_owned(), scheduled_loop("loop"));
    assert_eq!(
        vec![RowBadge::Tasks(1), RowBadge::Watchers(1)],
        live_work_badges(&agent).collect::<Vec<_>>()
    );
    agent.session.bg_tasks.get_mut("task").unwrap().status = BgTaskStatus::Done;
    let row = row(agent);
    assert_eq!(
        (RowState::Working, vec![RowBadge::Watchers(1)]),
        (row.state, row.badges)
    );
    assert!(row.last_change_at.elapsed().unwrap() >= Duration::from_secs(60));
}

#[test]
fn subagent_and_workflow_only_keep_existing_idle_classification() {
    let mut agent = agent();
    agent
        .subagent_sessions
        .insert("child".to_owned(), running_subagent_info("child"));
    agent.workflow_runs.push(workflow("active"));
    let row = row(agent);
    assert_eq!(
        (
            RowState::Idle,
            vec![RowBadge::Subagents(1), RowBadge::Workflows(1)]
        ),
        (row.state, row.badges)
    );
}
