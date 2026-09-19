// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared post-compaction reminder helpers (host-agnostic).
//!
//! Lives at the crate root rather than under a compaction-style submodule
//! because it is consumed by *both* compaction styles and both harnesses:
//!
//! - Grok chat intra FullReplace ([`crate::intra_compaction`]) and inter
//!   (appends after sampling via [`append_reminder_block`])
//! - distill full-replace ([`crate::code_compaction`] assemble's
//!   `system_reminder`)
//!
//! **What lives here:** pure formatting of the **common** active-agent
//! sections — Running Background Tasks (bash/monitor, scheduled loops,
//! live workflows), TODO List, and Running Subagents — plus
//! `<system-reminder>` wrapping and summary append.
//!
//! **What stays in the product host:** snapshotting, tool-name resolution, and harness-only
//! sections (files, AGENTS.md, skills, MCP, memory). Callers pass **borrowed
//! views** (`&str` over live state) so long fields (commands, todo content,
//! descriptions, ids) are not cloned just to format.
//!
//! KEEP IN SYNC: the exact wording of these sections is a compatibility
//! surface — downstream mirrors reproduce it verbatim (grep for
//! `format_section_running_subagents` / `format_section_background_tasks`
//! and `section_todo_list` mirrors). Update them when changing any wording
//! here.

// ---------------------------------------------------------------------------
// Borrowed views over harness live state (no long-string clones)
// ---------------------------------------------------------------------------

/// Model-facing poll/cancel tool names from the current toolset.
/// Never hard-code: a client manifest can rename them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubagentToolNames<'a> {
    pub poll: &'a str,
    pub cancel: &'a str,
}

/// Status of a todo item in the post-compaction reminder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::Pending => "[pending]",
            Self::InProgress => "[in_progress]",
            Self::Completed => "[completed]",
            Self::Cancelled => "[cancelled]",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TodoItem<'a> {
    pub id: &'a str,
    pub content: &'a str,
    pub status: TodoStatus,
}

/// Still-running background task. `task_id` is rendered verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackgroundTask<'a> {
    pub task_id: &'a str,
    pub command: &'a str,
    /// Parenthetical status (typically `"running"`).
    pub status: &'a str,
    pub tool_name: Option<&'a str>,
}

/// Still-running sub-agent. `subagent_id` is rendered verbatim.
///
/// `subagent_type` / `description` are optional so chat (no type, optional
/// desc) and build (both present) share one line format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunningSubagent<'a> {
    pub subagent_id: &'a str,
    pub subagent_type: Option<&'a str>,
    pub description: Option<&'a str>,
    pub elapsed_secs: u64,
}

/// Still-registered scheduled loop (`/loop` / scheduler). `task_id` is rendered
/// verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledLoop<'a> {
    pub task_id: &'a str,
    pub interval: &'a str,
    pub next_fire_at: &'a str,
    pub prompt: &'a str,
    pub recurring: bool,
    pub durable: bool,
}

/// Still-live workflow run. `run_id` is rendered verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkflowRun<'a> {
    pub name: &'a str,
    pub run_id: &'a str,
    pub status: &'a str,
    pub objective: Option<&'a str>,
    pub current_phase: Option<&'a str>,
    pub agents_used: u64,
    pub agent_budget: Option<u64>,
    pub elapsed_secs: u64,
}

/// Borrowed active-agent state for reminder rendering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActiveAgentReminderState<'a> {
    pub running_commands: &'a [BackgroundTask<'a>],
    pub todos: &'a [TodoItem<'a>],
    pub running_subagents: &'a [RunningSubagent<'a>],
    pub scheduled_loops: &'a [ScheduledLoop<'a>],
    pub workflows: &'a [WorkflowRun<'a>],
    pub workflow_tool: Option<&'a str>,
}

impl ActiveAgentReminderState<'_> {
    pub fn is_empty(&self) -> bool {
        self.running_commands.is_empty()
            && self.running_subagents.is_empty()
            && self.scheduled_loops.is_empty()
            && self.workflows.is_empty()
            && !self.has_actionable_todos()
    }

    pub fn has_actionable_todos(&self) -> bool {
        self.todos.iter().any(|t| t.status.is_actionable())
    }
}

// ---------------------------------------------------------------------------
// Section formatters
// ---------------------------------------------------------------------------

/// `## Running Background Tasks` for bash/monitor commands only, or `None`
/// when empty. Prefer [`section_running_background`] when loops / workflows
/// should share this heading. Subagents stay on [`section_running_subagents`].
pub fn section_background_tasks(tasks: &[BackgroundTask<'_>]) -> Option<String> {
    section_running_background(
        &ActiveAgentReminderState {
            running_commands: tasks,
            ..Default::default()
        },
        None,
    )
}

fn format_background_task_line(t: &BackgroundTask<'_>) -> String {
    match t.tool_name {
        Some(tool) => format!(
            "- \"{}\": `{}` ({}, {})",
            t.task_id, t.command, t.status, tool
        ),
        None => format!("- \"{}\": `{}` ({})", t.task_id, t.command, t.status),
    }
}

/// One `## Running Background Tasks` block for bash/monitor commands and
/// scheduled loops. Workflows stay on [`section_workflows`].
pub fn section_running_background(
    state: &ActiveAgentReminderState<'_>,
    _subagent_tools: Option<&SubagentToolNames<'_>>,
) -> Option<String> {
    let mut lines: Vec<String> = state
        .running_commands
        .iter()
        .map(format_background_task_line)
        .collect();
    lines.extend(state.scheduled_loops.iter().map(format_scheduled_loop_line));
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Running Background Tasks\n\
         These tasks are still running:\n{}",
        lines.join("\n")
    ))
}

/// `## TODO List` for actionable items, or `None` when none. Completed/
/// cancelled collapse to a count trailer.
pub fn section_todo_list(todos: &[TodoItem<'_>]) -> Option<String> {
    let active: Vec<_> = todos
        .iter()
        .filter(|t| t.status.is_actionable())
        .map(|t| format!("- {} {}: {}", t.status.tag(), t.id, t.content))
        .collect();
    if active.is_empty() {
        return None;
    }
    let completed = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let cancelled = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Cancelled)
        .count();
    let trailer = match (completed, cancelled) {
        (0, 0) => String::new(),
        (c, 0) => format!("\n({c} completed)"),
        (0, k) => format!("\n({k} cancelled)"),
        (c, k) => format!("\n({c} completed, {k} cancelled)"),
    };
    Some(format!(
        "## TODO List\n\
         This is your task list from before the conversation was compacted — it is still \
         active. Keep working through the items below and update their status as you make \
         progress:\n{}{trailer}",
        active.join("\n"),
    ))
}

/// `## Running Subagents`, or `None` when empty. Omit entirely when tool
/// names cannot be resolved rather than point at wrong names.
pub fn section_running_subagents(
    subagents: &[RunningSubagent<'_>],
    tools: &SubagentToolNames<'_>,
) -> Option<String> {
    if subagents.is_empty() {
        return None;
    }
    let lines = subagents
        .iter()
        .map(format_subagent_line)
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "## Running Subagents\n\
         These subagents were launched before this compaction and are still running. \
         Use `{}` with the subagent_id to check their status or retrieve results. \
         Use `{}` with the subagent_id to cancel a subagent.\n{lines}",
        tools.poll, tools.cancel
    ))
}

fn format_subagent_line(s: &RunningSubagent<'_>) -> String {
    // Same shape as bash/monitor/loops: `- "id": `text` (status, kind)`.
    let command = collapsed_ws(
        s.description
            .unwrap_or(s.subagent_type.unwrap_or("subagent")),
    );
    let kind = s.subagent_type.unwrap_or("subagent");
    format!(
        "- \"{}\": `{}` (running for {}s, {})",
        s.subagent_id, command, s.elapsed_secs, kind
    )
}

/// `## Scheduled Loops` is no longer a standalone section; loops render under
/// [`section_running_background`]. Kept so downstream mirrors that still call
/// this name compile until they switch.
pub fn section_scheduled_loops(loops: &[ScheduledLoop<'_>]) -> Option<String> {
    section_running_background(
        &ActiveAgentReminderState {
            scheduled_loops: loops,
            ..Default::default()
        },
        None,
    )
}

fn format_scheduled_loop_line(loop_task: &ScheduledLoop<'_>) -> String {
    // Same shape as bash/monitor: `- "id": `command` (status, kind)`.
    let prompt = collapsed_ws(loop_task.prompt);
    let status = if loop_task.recurring {
        format!("scheduler runs {}", loop_task.interval)
    } else {
        "scheduler one-shot".to_string()
    };
    format!(
        "- \"{}\": `{}` ({}, next {})",
        loop_task.task_id, prompt, status, loop_task.next_fire_at
    )
}

/// `## Running Workflows`, or `None` when empty.
pub fn section_workflows(
    workflows: &[WorkflowRun<'_>],
    workflow_tool: Option<&str>,
) -> Option<String> {
    if workflows.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = workflows.iter().map(format_workflow_line).collect();
    match workflow_tool {
        Some(tool) => lines.push(format!(
            "  Use `{tool}` to inspect or resume a paused run. Keep run ids internal."
        )),
        None => lines.push(
            "  Keep workflow run ids internal — the user knows runs by display name.".to_string(),
        ),
    }
    Some(format!(
        "## Running Workflows\n\
         These workflows are currently running:\n{}",
        lines.join("\n")
    ))
}

fn format_workflow_line(run: &WorkflowRun<'_>) -> String {
    let mut head = format!(
        "- Workflow '{}' (run id `{}`) — status: {}",
        run.name, run.run_id, run.status
    );
    if let Some(phase) = run.current_phase {
        head.push_str(", phase: ");
        head.push_str(phase);
    }
    head.push_str(&format!(" (running for {}s)", run.elapsed_secs));
    let mut lines = vec![head];
    if let Some(objective) = run.objective.filter(|o| !o.is_empty()) {
        lines.push(format!("  Objective: {}", collapsed_ws(objective)));
    }
    match run.agent_budget {
        Some(budget) => lines.push(format!(
            "  Agents: {} of {} budget",
            run.agents_used, budget
        )),
        None if run.agents_used > 0 => {
            lines.push(format!("  Agents: {}", run.agents_used));
        }
        None => {}
    }
    lines.join("\n")
}

fn collapsed_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Common sections: Running Background Tasks (commands, loops, workflows)
/// → TODO → Running Subagents.
/// Empty kinds omitted; subagents also omitted when `subagent_tools` is `None`.
pub fn format_active_agent_sections(
    state: &ActiveAgentReminderState<'_>,
    subagent_tools: Option<&SubagentToolNames<'_>>,
) -> Vec<String> {
    let mut sections = Vec::with_capacity(3);
    if let Some(s) = section_running_background(state, subagent_tools) {
        sections.push(s);
    }
    if let Some(s) = section_workflows(state.workflows, state.workflow_tool) {
        sections.push(s);
    }
    if let Some(s) = section_todo_list(state.todos) {
        sections.push(s);
    }
    if let Some(tools) = subagent_tools
        && let Some(s) = section_running_subagents(state.running_subagents, tools)
    {
        sections.push(s);
    }
    sections
}

/// Wrap non-empty sections in `<system-reminder>…</system-reminder>`.
pub fn wrap_system_reminder(sections: impl IntoIterator<Item = impl AsRef<str>>) -> Option<String> {
    let mut body = String::new();
    for s in sections {
        let s = s.as_ref();
        if s.trim().is_empty() {
            continue;
        }
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(s);
    }
    if body.is_empty() {
        None
    } else {
        Some(format!("<system-reminder>\n{body}\n</system-reminder>"))
    }
}

/// Full active-agent-state `<system-reminder>`, or `None` when nothing to preserve.
pub fn format_active_agent_reminder(
    state: &ActiveAgentReminderState<'_>,
    subagent_tools: Option<&SubagentToolNames<'_>>,
) -> Option<String> {
    wrap_system_reminder(format_active_agent_sections(state, subagent_tools))
}

// ---------------------------------------------------------------------------
// Summary injection
// ---------------------------------------------------------------------------

/// Append a trailing block to a compaction summary, separated by a blank line.
/// Returns `summary` unchanged when `reminder` is `None` or blank.
pub fn append_reminder_block(summary: String, reminder: Option<&str>) -> String {
    match reminder {
        Some(reminder) if !reminder.trim().is_empty() => format!("{summary}\n\n{reminder}"),
        _ => summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_native() -> SubagentToolNames<'static> {
        SubagentToolNames {
            poll: "get_task_output",
            cancel: "kill_task",
        }
    }

    fn tools_renamed() -> SubagentToolNames<'static> {
        SubagentToolNames {
            poll: "get_command_or_subagent_output",
            cancel: "kill_command_or_subagent",
        }
    }

    #[test]
    fn empty_state_is_none() {
        assert!(
            format_active_agent_reminder(
                &ActiveAgentReminderState::default(),
                Some(&tools_native())
            )
            .is_none()
        );
    }

    #[test]
    fn missing_tool_names_omits_subagent_section_only() {
        let agents = [RunningSubagent {
            subagent_id: "sa-1",
            subagent_type: None,
            description: Some("x"),
            elapsed_secs: 1,
        }];
        let state = ActiveAgentReminderState {
            running_subagents: &agents,
            ..Default::default()
        };
        assert!(format_active_agent_reminder(&state, None).is_none());

        let cmds = [BackgroundTask {
            task_id: "bg-1",
            command: "npm run dev",
            status: "running",
            tool_name: Some("run_terminal_command"),
        }];
        let state = ActiveAgentReminderState {
            running_commands: &cmds,
            ..Default::default()
        };
        let out = format_active_agent_reminder(&state, None).expect("reminder");
        assert!(out.contains("## Running Background Tasks"));
        assert!(!out.contains("subagent_id:"));
    }

    #[test]
    fn renders_chat_style_subagent_ids_verbatim() {
        let agents = [
            RunningSubagent {
                subagent_id: "019ea7f0-cb66-7aa2-9a09-488a3a795795",
                subagent_type: None,
                description: Some("deploy staging"),
                elapsed_secs: 42,
            },
            RunningSubagent {
                subagent_id: "sa-2",
                subagent_type: None,
                description: None,
                elapsed_secs: 5,
            },
        ];
        let state = ActiveAgentReminderState {
            running_subagents: &agents,
            ..Default::default()
        };
        let out = format_active_agent_reminder(&state, Some(&tools_native())).expect("reminder");
        assert!(out.starts_with("<system-reminder>"));
        assert!(out.ends_with("</system-reminder>"));
        assert!(out.contains(
            "- \"019ea7f0-cb66-7aa2-9a09-488a3a795795\": `deploy staging` (running for 42s, subagent)"
        ));
        assert!(out.contains("- \"sa-2\": `subagent` (running for 5s, subagent)"));
        assert!(!out.contains("task-019ea7f0"));
    }

    #[test]
    fn renders_build_style_subagent_with_type() {
        let agents = [RunningSubagent {
            subagent_id: "sub-1",
            subagent_type: Some("explore"),
            description: Some("find files"),
            elapsed_secs: 5,
        }];
        let state = ActiveAgentReminderState {
            running_subagents: &agents,
            ..Default::default()
        };
        let out = format_active_agent_reminder(&state, Some(&tools_renamed())).expect("reminder");
        assert!(out.contains("- \"sub-1\": `find files` (running for 5s, explore)"));
    }

    #[test]
    fn uses_renamed_tool_names_verbatim() {
        let agents = [RunningSubagent {
            subagent_id: "sa-1",
            subagent_type: None,
            description: Some("x"),
            elapsed_secs: 1,
        }];
        let state = ActiveAgentReminderState {
            running_subagents: &agents,
            ..Default::default()
        };
        let out = format_active_agent_reminder(&state, Some(&tools_renamed())).expect("reminder");
        assert!(out.contains("get_command_or_subagent_output"));
        assert!(!out.contains("get_task_output"));
    }

    #[test]
    fn renders_background_tasks() {
        let cmds = [
            BackgroundTask {
                task_id: "019f1723-a9f0-76f2-98ae-56af965922f6",
                command: "npm run dev",
                status: "running",
                tool_name: Some("run_terminal_command"),
            },
            BackgroundTask {
                task_id: "bg-2",
                command: "cargo watch -x test",
                status: "running",
                tool_name: None,
            },
        ];
        let state = ActiveAgentReminderState {
            running_commands: &cmds,
            ..Default::default()
        };
        let out = format_active_agent_reminder(&state, None).expect("reminder");
        assert!(out.contains(
            "- \"019f1723-a9f0-76f2-98ae-56af965922f6\": `npm run dev` (running, run_terminal_command)"
        ));
        assert!(out.contains("- \"bg-2\": `cargo watch -x test` (running)"));
    }

    #[test]
    fn renders_todo_list_without_tool_names() {
        let todos = [
            TodoItem {
                id: "1",
                content: "scaffold the app",
                status: TodoStatus::Completed,
            },
            TodoItem {
                id: "2",
                content: "wire the API",
                status: TodoStatus::InProgress,
            },
            TodoItem {
                id: "3",
                content: "write tests",
                status: TodoStatus::Pending,
            },
        ];
        let state = ActiveAgentReminderState {
            todos: &todos,
            ..Default::default()
        };
        let out = format_active_agent_reminder(&state, None).expect("reminder");
        assert!(out.contains("- [in_progress] 2: wire the API"));
        assert!(out.contains("- [pending] 3: write tests"));
        assert!(out.contains("(1 completed)"));
        assert!(!out.contains("scaffold the app"));
    }

    #[test]
    fn only_completed_todos_is_none() {
        let todos = [TodoItem {
            id: "1",
            content: "done",
            status: TodoStatus::Completed,
        }];
        let state = ActiveAgentReminderState {
            todos: &todos,
            ..Default::default()
        };
        assert!(format_active_agent_reminder(&state, None).is_none());
    }

    #[test]
    fn section_order_background_loops_workflows_todo_subagent() {
        let cmds = [BackgroundTask {
            task_id: "t1",
            command: "npm run dev",
            status: "running",
            tool_name: None,
        }];
        let loops = [ScheduledLoop {
            task_id: "loop1",
            interval: "every 20 minutes",
            next_fire_at: "in 12m",
            prompt: "monitor job",
            recurring: true,
            durable: true,
        }];
        let workflows = [WorkflowRun {
            name: "review-changes",
            run_id: "wf-1",
            status: "active",
            objective: Some("review the PR"),
            current_phase: Some("Smoke"),
            agents_used: 3,
            agent_budget: Some(128),
            elapsed_secs: 12,
        }];
        let todos = [TodoItem {
            id: "2",
            content: "wire the API",
            status: TodoStatus::InProgress,
        }];
        let agents = [RunningSubagent {
            subagent_id: "sa-1",
            subagent_type: None,
            description: Some("deploy staging"),
            elapsed_secs: 1,
        }];
        let state = ActiveAgentReminderState {
            running_commands: &cmds,
            todos: &todos,
            running_subagents: &agents,
            scheduled_loops: &loops,
            workflows: &workflows,
            workflow_tool: Some("workflow"),
        };
        let out = format_active_agent_reminder(&state, Some(&tools_native())).expect("reminder");
        let bg = out.find("## Running Background Tasks").expect("bg");
        let todo = out.find("## TODO List").expect("todo");
        let sub = out.find("## Running Subagents").expect("sub");
        assert!(bg < todo && todo < sub, "order wrong:\n{out}");
        assert!(!out.contains("## Scheduled Loops"), "{out}");
        assert!(!out.contains("## Active Workflows"), "{out}");
        assert!(out.contains("## Running Workflows"), "{out}");
        assert!(
            out.contains("01a046ad3877") || out.contains("loop1"),
            "{out}"
        );
        assert!(out.contains("review-changes"), "{out}");
        assert!(out.contains("- \"sa-1\":"), "{out}");
    }

    #[test]
    fn renders_scheduled_loops_and_workflows_verbatim() {
        let loops = [ScheduledLoop {
            task_id: "01a046ad3877",
            interval: "every 20 minutes",
            next_fire_at: "in 12m",
            prompt: "monitor job\nand report",
            recurring: true,
            durable: true,
        }];
        let workflows = [WorkflowRun {
            name: "review-changes",
            run_id: "wf-1",
            status: "user_paused",
            objective: Some("review the PR"),
            current_phase: Some("Diff"),
            agents_used: 2,
            agent_budget: Some(16),
            elapsed_secs: 90,
        }];
        let state = ActiveAgentReminderState {
            scheduled_loops: &loops,
            workflows: &workflows,
            workflow_tool: Some("workflow"),
            ..Default::default()
        };
        let out = format_active_agent_reminder(&state, None).expect("reminder");
        assert!(out.contains(
            "- \"01a046ad3877\": `monitor job and report` (scheduler runs every 20 minutes, next in 12m)"
        ));
        assert!(out.contains(
            "- Workflow 'review-changes' (run id `wf-1`) — status: user_paused, phase: Diff (running for 90s)"
        ));
        assert!(out.contains("Objective: review the PR"));
        assert!(out.contains("Agents: 2 of 16 budget"));
        assert!(out.contains("## Running Workflows"));
        assert!(out.contains("These workflows are currently running:"));
        assert!(out.contains("Use `workflow` to inspect or resume a paused run"));
    }

    #[test]
    fn wrap_system_reminder_joins_and_skips_blank() {
        let out = wrap_system_reminder(["## A\nx", "", "  ", "## B\ny"]).expect("wrapped");
        assert_eq!(
            out,
            "<system-reminder>\n## A\nx\n\n## B\ny\n</system-reminder>"
        );
        assert!(wrap_system_reminder(std::iter::empty::<&str>()).is_none());
    }

    #[test]
    fn appends_after_blank_line() {
        assert_eq!(
            append_reminder_block("SUMMARY".to_string(), Some("REMINDER")),
            "SUMMARY\n\nREMINDER"
        );
    }

    #[test]
    fn append_noop_when_none_or_blank() {
        assert_eq!(
            append_reminder_block("SUMMARY".to_string(), None),
            "SUMMARY"
        );
        assert_eq!(
            append_reminder_block("SUMMARY".to_string(), Some("  \n\t ")),
            "SUMMARY"
        );
    }
}
