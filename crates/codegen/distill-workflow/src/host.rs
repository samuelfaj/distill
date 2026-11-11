// Modified for Distill by Samuel Fajreldines, 2026.
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// `sh()` gives a command this long before the host kills it, unless the script passes `timeout_ms`.
pub const DEFAULT_SH_TIMEOUT_MS: u64 = 600_000;
/// Bytes of stdout and of stderr a command result keeps; the host keeps the tail and flags the cut.
pub const MAX_SH_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOpts {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub capability_mode: Option<String>,
    #[serde(default)]
    pub isolation_worktree: bool,
    #[serde(default)]
    pub fork_context: bool,
    #[serde(default)]
    pub resume_from: Option<String>,
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub agent_id: String,
    pub success: bool,
    pub output: serde_json::Value,
    pub cancelled: bool,
    pub tokens_used: u64,
    pub duration_ms: u64,
}

/// Options of `sh(command, opts)` and `spawn_sh(command, opts)`. Unknown keys are rejected, so a typo cannot silently
/// run a command without its timeout or outside its directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShOpts {
    /// Milliseconds before the command is killed; [`DEFAULT_SH_TIMEOUT_MS`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Name shown in the run's progress display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Directory to run in, relative to the workflow's working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// What `sh()` returns. A command that exits non-zero, times out or is cancelled is still a result (`ok: false`),
/// never a script error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShResult {
    pub ok: bool,
    /// Exit code; absent when the command was killed (timeout, cancel, signal).
    pub code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_ms: u64,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    /// The script called `cancel(ticket)` on it.
    #[serde(default)]
    pub cancelled: bool,
}

/// Marks a host call the script holds a ticket for (`spawn_agent()`, `spawn_sh()`) instead of blocking on it.
#[derive(Debug, Clone)]
pub struct TicketHandle {
    /// The ticket the script got, e.g. `"t3"`; [`WorkflowHostRequest::AwaitTickets`] names the same ids.
    pub id: String,
    /// Cancelled when the script calls `cancel(ticket)`: the host stops this one call and still replies, with a
    /// result marked `cancelled`.
    pub cancel: CancellationToken,
}

/// The timeout of a `wait_any(tickets, timeout_ms)`. The host owns the clock: it sends on `fire` once `ms` have passed,
/// and dropping `fire` means the wait never times out.
#[derive(Debug)]
pub struct WaitTimeout {
    pub ms: u64,
    pub fire: oneshot::Sender<()>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BudgetState {
    pub total: Option<u64>,
    pub spent: u64,
    pub reserved: u64,
    pub remaining: Option<u64>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum HostError {
    #[error("workflow agent-call quota exceeded: requested {requested}, maximum {maximum}")]
    AgentCallQuotaExceeded { requested: u64, maximum: u64 },
    #[error("workflow token budget exceeded")]
    BudgetExceeded,
    #[error("workflow cancelled")]
    Cancelled,
    #[error("unsupported in this context: {0}")]
    Unsupported(String),
    #[error("host failure: {0}")]
    Failed(String),
}

#[derive(Debug)]
pub enum WorkflowHostRequest {
    ReserveAgentCalls {
        count: u64,
        reply: oneshot::Sender<Result<(), HostError>>,
    },
    ReleaseAgentCalls {
        count: u64,
        reply: oneshot::Sender<Result<(), HostError>>,
    },
    SpawnAgent {
        opts: AgentOpts,
        /// `None` for `agent()` and `parallel()`, which block until the reply.
        ticket: Option<TicketHandle>,
        reply: oneshot::Sender<Result<AgentResult, HostError>>,
    },
    /// `sh()` / `spawn_sh()`: run a shell command without a model.
    RunCommand {
        command: String,
        opts: ShOpts,
        /// `None` for `sh()`, which blocks until the reply.
        ticket: Option<TicketHandle>,
        reply: oneshot::Sender<Result<ShResult, HostError>>,
    },
    /// The script is now blocked until these tickets reply: one of them (`all: false`, `wait_any`) or every one
    /// (`all: true`, `wait_all` and the drain at script end). Sent once per blocking wait, and only for tickets
    /// that have not replied yet. A host needs it for the timeout alone; a simulated host uses it to advance its clock.
    AwaitTickets {
        tickets: Vec<String>,
        all: bool,
        timeout: Option<WaitTimeout>,
    },
    Phase {
        title: String,
        replayed: bool,
    },
    Log {
        message: String,
        replayed: bool,
    },
    Telemetry {
        name: String,
        fields: serde_json::Value,
        replayed: bool,
    },
    BudgetQuery {
        reply: oneshot::Sender<Result<BudgetState, HostError>>,
    },
    RenderTemplate {
        name: String,
        vars: serde_json::Value,
        reply: oneshot::Sender<Result<String, HostError>>,
    },
    WriteScratchFile {
        name: String,
        content: String,
        reply: oneshot::Sender<Result<String, HostError>>,
    },
    ReadScratchFile {
        name: String,
        reply: oneshot::Sender<Result<String, HostError>>,
    },
    GitDiffSince {
        commit: String,
        reply: oneshot::Sender<Result<String, HostError>>,
    },
}

impl WorkflowHostRequest {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ReserveAgentCalls { .. } => "reserve_agent_calls",
            Self::ReleaseAgentCalls { .. } => "release_agent_calls",
            Self::SpawnAgent { .. } => "spawn_agent",
            Self::RunCommand { .. } => "sh",
            Self::AwaitTickets { .. } => "await_tickets",
            Self::Phase { .. } => "phase",
            Self::Log { .. } => "log",
            Self::Telemetry { .. } => "telemetry",
            Self::BudgetQuery { .. } => "budget",
            Self::RenderTemplate { .. } => "render_template",
            Self::WriteScratchFile { .. } => "write_scratch_file",
            Self::ReadScratchFile { .. } => "read_scratch_file",
            Self::GitDiffSince { .. } => "git_diff_since",
        }
    }
}
