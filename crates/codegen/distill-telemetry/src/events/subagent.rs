//! Subagent and workflow product telemetry events.

use super::Outcome;
use serde::Serialize;

/// Which spawn path owns a subagent.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentOwnerKind {
    Task,
    Workflow,
    SchedulerLoop,
}

/// Which admission limit a spawn ran into.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentLimitKind {
    SessionConcurrent,
    WorkflowRunConcurrent,
}

/// What happened to the spawn that hit a limit.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentLimitDisposition {
    Queued,
    Failed,
}

#[derive(Serialize)]
pub struct SubagentLaunched {
    pub subagent_id: String,
    pub parent_session_id: String,
    pub subagent_type: String,
    pub owner: SubagentOwnerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    /// Time parked in the admission queue; absent if admitted immediately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_ms: Option<u64>,
    /// The session's running non-workflow subagents at launch, including this one; max per session is the session's peak concurrency.
    pub session_running: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    pub fork_context: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from: Option<String>,
    pub isolated_worktree: bool,
    pub mcp_inherited_count: u32,
    pub mcp_owned_count: u32,
    pub skills_inherited_count: u32,
}

#[derive(Serialize)]
pub struct SubagentCompleted {
    pub subagent_id: String,
    pub parent_session_id: String,
    pub owner: SubagentOwnerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub tool_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    // Spawn-phase durations (`crate::subagent_spawn`, the `grok_code_subagent_spawn_*` taxonomy); absent when a phase did not run
    // Populated through `SubagentSpawnTimer::write_event_phases`' single match, which fails to compile until a new phase is given a field below
    // Phases are hierarchical (agent_build and tool_setup nest in session_bootstrap); summing all of them double-counts
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_prepare_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_bootstrap_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_build_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_setup_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_to_first_turn_ms: Option<u64>,
}

#[derive(Serialize)]
pub struct SubagentLimitHit {
    pub parent_session_id: String,
    pub limit_kind: SubagentLimitKind,
    pub disposition: SubagentLimitDisposition,
    pub limit: u64,
    pub running: u32,
    /// A queued spawn counts itself; absent for the workflow pool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued: Option<u32>,
    pub owner: SubagentOwnerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_run_id: Option<String>,
}

impl SubagentLimitHit {
    /// The session pool's producer.
    pub fn session_concurrent(
        parent_session_id: String,
        disposition: SubagentLimitDisposition,
        limit: u64,
        running: u32,
        queue_depth: u32,
        owner: SubagentOwnerKind,
    ) -> Self {
        Self {
            parent_session_id,
            limit_kind: SubagentLimitKind::SessionConcurrent,
            disposition,
            limit,
            running,
            queued: Some(queue_depth),
            owner,
            workflow_run_id: None,
        }
    }

    /// The workflow pool's producer: waiters block on the run's semaphore, so there is no queue depth to report.
    pub fn workflow_run_concurrent(
        parent_session_id: String,
        workflow_run_id: String,
        limit: u64,
        slots_in_use: u32,
    ) -> Self {
        Self {
            parent_session_id,
            limit_kind: SubagentLimitKind::WorkflowRunConcurrent,
            disposition: SubagentLimitDisposition::Queued,
            limit,
            running: slots_in_use,
            queued: None,
            owner: SubagentOwnerKind::Workflow,
            workflow_run_id: Some(workflow_run_id),
        }
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitWaitOutcome {
    Recovered,
    BudgetSpent,
    Unresolved,
}

/// Emitted once per inner `process_conversation_turn`, so one `turn_number` can carry several rows; do not blindly GROUP BY turn_number.
#[derive(Serialize)]
pub struct SubagentRateLimitWaited {
    /// Resubmits (waits) this turn, excluding the initial send.
    pub attempts: u32,
    pub max_attempts: u32,
    pub waited_ms: u64,
    pub budget_ms: u64,
    pub outcome: RateLimitWaitOutcome,
}

/// Where a workflow script came from.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSourceKind {
    Builtin,
    File,
    Inline,
}

/// One workflow execution episode began (fresh launch or resume).
#[derive(Serialize)]
pub struct WorkflowRunStarted {
    pub run_id: String,
    pub parent_session_id: String,
    pub source: WorkflowSourceKind,
    /// Built-in workflow names only; user script names stay local.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_budget: Option<u64>,
    /// Effective cap, after the CPU clamp.
    pub max_concurrent_agents: u32,
    pub resumed: bool,
}

/// The run tracker's status labels, plus `superseded` for an episode whose run a quick resume took over.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunEndStatus {
    Active,
    UserPaused,
    BackOffPaused,
    NoProgressPaused,
    InfraPaused,
    Blocked,
    BudgetLimited,
    Interrupted,
    Complete,
    Failed,
    Cancelled,
    Superseded,
}

#[derive(Serialize)]
pub struct WorkflowRunEnded {
    pub run_id: String,
    pub parent_session_id: String,
    pub status: WorkflowRunEndStatus,
    /// Cumulative across the run's episodes.
    pub duration_ms: u64,
    /// Cumulative across the run's episodes.
    pub agents_used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_budget: Option<u64>,
    /// This episode only.
    pub agents_failed: u32,
    /// This episode only.
    pub peak_concurrent_agents: u32,
    /// This episode only.
    pub slot_waits: u32,
    /// This episode only.
    pub slot_wait_ms_total: u64,
    /// This episode only.
    pub slot_wait_ms_max: u64,
}
