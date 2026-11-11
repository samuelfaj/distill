// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
use distill_tools::implementations::distill::task::types::{
    ModelOverrideProvenance, SubagentCancelRequest, SubagentCancelTarget, SubagentEvent,
    SubagentOwner, SubagentRequest, SubagentRuntimeOverrides,
};
use distill_workflow::{
    AgentOpts, AgentResult, BudgetState, DEFAULT_SH_TIMEOUT_MS, HostError, MAX_SH_OUTPUT_BYTES,
    ShOpts, ShResult, TicketHandle, WaitTimeout, WorkflowHostRequest,
};
use distill_workspace::permission::{AccessKind, Decision, PermissionHandle, PermissionRequest};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::notify::WorkflowNotifySender;
use super::schema_contract::{
    SCHEMA_CONTRACT_RETRIES, compile_contract_schema, contract_prompt, validate_contract_output,
};
use super::tracker::WorkflowTracker;

pub(crate) const WORKFLOW_MAX_AGENT_RUNS: u32 =
    (distill_workflow::MAX_AGENT_BUDGET as u32) * (SCHEMA_CONTRACT_RETRIES + 1);
pub(crate) const DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS: usize = 32;

/// The configured cap clamped to the machine's parallelism, so small hosts run fewer agents at once.
pub(crate) fn workflow_max_concurrent_agents(configured: usize) -> usize {
    workflow_max_concurrent_agents_from(
        configured,
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS),
    )
}

fn workflow_max_concurrent_agents_from(configured: usize, parallelism: usize) -> usize {
    let clamp = parallelism.max(2);
    let requested = configured.max(1);
    // Logged for the default too: a small host silently running fewer than the default agents per run would otherwise be invisible to operators
    if requested > clamp {
        tracing::info!(
            requested,
            clamped_to = clamp,
            "workflow agent concurrency clamped to machine parallelism"
        );
    }
    requested.min(clamp)
}
pub(crate) const WORKFLOW_MAX_SCRIPT_TELEMETRY_EVENTS: u32 = 64;
pub(crate) const WORKFLOW_MAX_SCRATCH_FILES: usize = 64;
pub(crate) const WORKFLOW_MAX_SCRATCH_FILE_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const WORKFLOW_MAX_SCRATCH_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const WORKFLOW_MAX_AGENT_PROMPT_BYTES: usize = 1024 * 1024;
const WORKFLOW_MAX_TEMPLATE_OUTPUT_BYTES: usize = 1024 * 1024;
const WORKFLOW_MAX_PHASE_BYTES: usize = 256;
const WORKFLOW_MAX_LOG_BYTES: usize = 4 * 1024;
const WORKFLOW_CHILD_DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
const WORKFLOW_MAX_SCRATCH_NAME_BYTES: usize = 255;
const WORKFLOW_MAX_COMMAND_BYTES: usize = 64 * 1024;
const WORKFLOW_MAX_COMMAND_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
/// How long a finished command's pipes may stay open (a background grandchild holds them) before its tree is killed.
const WORKFLOW_COMMAND_PIPE_DRAIN: Duration = Duration::from_secs(2);
const SCRATCH_ARTIFACT_ROOT: &str = "scratch";

pub(crate) type TelemetryHook = Arc<dyn Fn(&str, &serde_json::Value, bool) + Send + Sync>;

/// Per-episode agent counters, reported on `WorkflowRunEnded`.
#[derive(Debug, Default)]
pub(crate) struct WorkflowAgentStats {
    pub peak_concurrent: AtomicU32,
    pub agents_failed: AtomicU32,
    pub slot_waits: AtomicU32,
    pub slot_wait_ms_total: AtomicU64,
    pub slot_wait_ms_max: AtomicU64,
}

pub(crate) struct WorkflowHostParams {
    pub run_id: String,
    /// Agents this run keeps live at once; wider fan-outs queue in order.
    pub max_concurrent_agents: usize,
    pub cwd: PathBuf,
    pub scratch_dir: PathBuf,
    pub tracker: Arc<parking_lot::Mutex<WorkflowTracker>>,
    pub store: super::store::WorkflowRunStore,
    pub notify: WorkflowNotifySender,
    pub subagent_event_tx:
        mpsc::UnboundedSender<distill_tools::implementations::distill::task::types::SubagentEvent>,
    pub parent_session_id: String,
    /// The session's permission manager: `sh()` commands are decided by it, like an agent's terminal commands.
    pub permissions: PermissionHandle,
    pub allow_fork_context: bool,
    pub effort: Option<distill_sampling_types::ReasoningEffort>,
    pub templates: std::collections::HashMap<String, String>,
    pub telemetry: TelemetryHook,
    pub stats: Arc<WorkflowAgentStats>,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostDrainOutcome {
    Drained,
    TimedOut,
}

pub(crate) fn spawn_workflow_host_service(
    params: WorkflowHostParams,
    mut rx: mpsc::UnboundedReceiver<WorkflowHostRequest>,
) -> (
    tokio::task::JoinHandle<()>,
    oneshot::Receiver<HostDrainOutcome>,
) {
    let (drained_tx, drained_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let service = Arc::new(HostService {
            active_agents: AtomicU32::new(0),
            agent_runs: AtomicU32::new(0),
            script_telemetry_events: AtomicU32::new(0),
            scratch_io: tokio::sync::Mutex::new(()),
            agent_slots: tokio::sync::Semaphore::new(params.max_concurrent_agents.max(1)),
            params,
        });
        loop {
            let req = tokio::select! {
                req = rx.recv() => match req {
                    Some(req) => req,
                    None => break,
                },
                _ = service.params.cancel.cancelled() => {
                    while let Ok(req) = rx.try_recv() {
                        reply_cancelled(req);
                    }
                    break;
                }
            };
            service.clone().dispatch(req);
        }
        let drain_outcome = service.cancel_and_drain_children().await;
        let _ = drained_tx.send(drain_outcome);
    });
    (handle, drained_rx)
}

fn reply_cancelled(req: WorkflowHostRequest) {
    use WorkflowHostRequest as R;
    match req {
        R::ReserveAgentCalls { reply, .. } | R::ReleaseAgentCalls { reply, .. } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::SpawnAgent { reply, .. } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::RunCommand { reply, .. } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::BudgetQuery { reply } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        R::RenderTemplate { reply, .. }
        | R::WriteScratchFile { reply, .. }
        | R::ReadScratchFile { reply, .. }
        | R::GitDiffSince { reply, .. } => {
            let _ = reply.send(Err(HostError::Cancelled));
        }
        // Dropping the timeout is "never fires"; the tickets it waited on are answered as cancelled themselves.
        R::AwaitTickets { .. } | R::Phase { .. } | R::Log { .. } | R::Telemetry { .. } => {}
    }
}

/// The last [`MAX_SH_OUTPUT_BYTES`] of one of a command's output streams, and how much it wrote in all.
#[derive(Default)]
struct OutputTail {
    bytes: Vec<u8>,
    total: u64,
}

impl OutputTail {
    fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        self.bytes.extend_from_slice(chunk);
        // Trim in batches so a chatty command does not shift the buffer on every read.
        if self.bytes.len() > MAX_SH_OUTPUT_BYTES.saturating_mul(2) {
            self.trim();
        }
    }

    fn trim(&mut self) {
        let excess = self.bytes.len().saturating_sub(MAX_SH_OUTPUT_BYTES);
        self.bytes.drain(..excess);
    }

    /// The text for the script and whether it was cut. A cut is also stated on the first line, because the text
    /// often goes straight into an agent prompt where the flag would not follow it.
    fn into_text(mut self, stream: &str) -> (String, bool) {
        self.trim();
        let text = String::from_utf8_lossy(&self.bytes);
        if self.total > self.bytes.len() as u64 {
            (
                format!(
                    "[{stream} truncated: showing the last {} of {} bytes]\n{text}",
                    self.bytes.len(),
                    self.total
                ),
                true,
            )
        } else {
            (text.into_owned(), false)
        }
    }
}

type SharedTail = Arc<parking_lot::Mutex<OutputTail>>;

/// Reads a child's pipe until it closes. The tail is shared rather than returned so that what was read is kept even
/// when the reader has to be abandoned.
fn capture_tail(
    pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
) -> (SharedTail, tokio::task::JoinHandle<()>) {
    let tail = SharedTail::default();
    let sink = tail.clone();
    let reader = tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let Some(mut pipe) = pipe else {
            return;
        };
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => sink.lock().push(chunk.get(..read).unwrap_or_default()),
            }
        }
    });
    (tail, reader)
}

fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
    #[cfg(not(unix))]
    {
        let invocation = distill_config::shell::shell_command_argv(command);
        let mut cmd = tokio::process::Command::new(&invocation.program);
        cmd.args(&invocation.args).envs(invocation.env);
        cmd
    }
}

/// The result of a ticket the script cancelled before or while its command ran.
fn cancelled_command(duration_ms: u64) -> ShResult {
    ShResult {
        cancelled: true,
        duration_ms,
        ..Default::default()
    }
}

struct HostService {
    active_agents: AtomicU32,
    agent_runs: AtomicU32,
    script_telemetry_events: AtomicU32,
    scratch_io: tokio::sync::Mutex<()>,
    agent_slots: tokio::sync::Semaphore,
    params: WorkflowHostParams,
}

struct FinishOnce<'a> {
    host: &'a HostService,
    agent_id: String,
    finished: bool,
}

impl FinishOnce<'_> {
    fn finish(&mut self, state: &str, total_tokens: u64, total_duration: u64) {
        debug_assert!(!self.finished, "agent roster row finished twice");
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        if state == "failed" {
            // The roster is capped and survives resume, so failed rows cannot be counted from it; count here
            self.host
                .params
                .stats
                .agents_failed
                .fetch_add(1, Ordering::Relaxed);
        }
        self.host.params.tracker.lock().agent_finished(
            &self.host.params.run_id,
            &self.agent_id,
            state,
            total_tokens,
            total_duration,
        );
    }

    fn rebind(&mut self, new_agent_id: &str) {
        self.host.params.tracker.lock().rebind_agent_id(
            &self.host.params.run_id,
            &self.agent_id,
            new_agent_id,
        );
        self.agent_id = new_agent_id.to_string();
    }
}

impl HostService {
    fn dispatch(self: Arc<Self>, req: WorkflowHostRequest) {
        match req {
            WorkflowHostRequest::ReserveAgentCalls { count, reply } => {
                let reserved = self
                    .params
                    .tracker
                    .lock()
                    .reserve_agents(&self.params.run_id, count);
                let result = match reserved {
                    Ok(state) => {
                        if let Err(error) = self.params.store.persist_now(&state) {
                            self.params
                                .tracker
                                .lock()
                                .release_agents(&self.params.run_id, count);
                            Err(HostError::Failed(format!(
                                "workflow agent-budget reservation could not be persisted: {error}"
                            )))
                        } else {
                            Ok(())
                        }
                    }
                    Err((requested, maximum)) => {
                        Err(HostError::AgentCallQuotaExceeded { requested, maximum })
                    }
                };
                let _ = reply.send(result);
            }
            WorkflowHostRequest::ReleaseAgentCalls { count, reply } => {
                let released = self
                    .params
                    .tracker
                    .lock()
                    .release_agents(&self.params.run_id, count);
                let result = match released {
                    Some(state) => {
                        if let Err(error) = self.params.store.persist_now(&state) {
                            tracing::warn!(
                                run_id = %self.params.run_id,
                                %error,
                                "workflow agent-budget release not persisted; keeping in-memory release"
                            );
                        }
                        Ok(())
                    }
                    None => Err(HostError::Failed(format!(
                        "workflow run not found for agent-budget release: {}",
                        self.params.run_id
                    ))),
                };
                let _ = reply.send(result);
            }
            WorkflowHostRequest::SpawnAgent {
                opts,
                ticket,
                reply,
            } => {
                let svc = self.clone();
                tokio::spawn(async move {
                    let result = svc.spawn_agent(opts, ticket).await;
                    let _ = reply.send(result);
                });
            }
            WorkflowHostRequest::RunCommand {
                command,
                opts,
                ticket,
                reply,
            } => {
                let svc = self.clone();
                tokio::spawn(async move {
                    let result = svc.run_command(command, opts, ticket).await;
                    let _ = reply.send(result);
                });
            }
            WorkflowHostRequest::AwaitTickets { timeout, .. } => {
                // The engine holds the ticket replies itself; the host only keeps the clock for the timeout.
                if let Some(WaitTimeout { ms, mut fire }) = timeout {
                    let cancel = self.params.cancel.clone();
                    tokio::spawn(async move {
                        let elapsed = tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(ms)) => true,
                            // The wait ended first.
                            _ = fire.closed() => false,
                            // Dropping the timer is "never fires": the cancelled tickets end the wait themselves.
                            _ = cancel.cancelled() => false,
                        };
                        if elapsed {
                            let _ = fire.send(());
                        }
                    });
                }
            }
            WorkflowHostRequest::Phase { title, replayed } => {
                if title.len() > WORKFLOW_MAX_PHASE_BYTES {
                    tracing::warn!(run_id = %self.params.run_id, "workflow phase title dropped: too large");
                    return;
                }
                let state = self
                    .params
                    .tracker
                    .lock()
                    .set_phase(&self.params.run_id, &title);
                if let Some(state) = state {
                    let elapsed = self.elapsed_ms();
                    let agents = self.active_agents.load(Ordering::Relaxed);
                    if replayed {
                        self.params.notify.emit_ephemeral(&state, elapsed, agents);
                    } else {
                        self.params.notify.emit(&state, elapsed, agents);
                    }
                }
            }
            WorkflowHostRequest::Log { message, replayed } => {
                if message.len() > WORKFLOW_MAX_LOG_BYTES {
                    tracing::warn!(run_id = %self.params.run_id, "workflow log dropped: too large");
                    return;
                }
                if !replayed {
                    tracing::info!(run_id = %self.params.run_id, "workflow log: {message}");
                    let state = self
                        .params
                        .tracker
                        .lock()
                        .log_message(&self.params.run_id, &message);
                    if let Some(state) = state {
                        let elapsed = self.elapsed_ms();
                        let agents = self.active_agents.load(Ordering::Relaxed);
                        self.params.notify.emit_ephemeral(&state, elapsed, agents);
                    }
                }
            }
            WorkflowHostRequest::Telemetry {
                name,
                fields,
                replayed,
            } => {
                if !replayed
                    && self.script_telemetry_events.fetch_add(1, Ordering::Relaxed)
                        < WORKFLOW_MAX_SCRIPT_TELEMETRY_EVENTS
                {
                    let host_fields = serde_json::json!({
                        "run_id": &self.params.run_id,
                        "script_event_name_bytes": name.len().min(64 * 1024),
                        "script_event_field_count": fields.as_object().map_or(0, serde_json::Map::len),
                    });
                    (self.params.telemetry)(
                        "workflow_script_event_suppressed",
                        &host_fields,
                        false,
                    );
                }
            }
            WorkflowHostRequest::BudgetQuery { reply } => {
                let _ = reply.send(Ok(self.budget_state()));
            }
            WorkflowHostRequest::RenderTemplate { name, vars, reply } => {
                let _ = reply.send(self.render_template(&name, &vars));
            }
            WorkflowHostRequest::WriteScratchFile {
                name,
                content,
                reply,
            } => {
                let svc = self.clone();
                tokio::spawn(async move {
                    let _ = reply.send(svc.write_scratch_file(&name, &content).await);
                });
            }
            WorkflowHostRequest::ReadScratchFile { name, reply } => {
                let svc = self.clone();
                tokio::spawn(async move {
                    let _ = reply.send(svc.read_scratch_file(&name).await);
                });
            }
            WorkflowHostRequest::GitDiffSince { commit, reply } => {
                let svc = self.clone();
                tokio::spawn(async move {
                    let _ = reply.send(svc.git_diff_since(&commit).await);
                });
            }
        }
    }

    fn budget_state(&self) -> BudgetState {
        let tracker = self.params.tracker.lock();
        let run = tracker.get(&self.params.run_id);
        let total = run.as_ref().and_then(|r| r.agent_budget);
        let spent = run.as_ref().map_or(0, |r| r.agents_used);
        BudgetState {
            total,
            spent,
            reserved: 0,
            remaining: total.map(|total| total.saturating_sub(spent)),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.params.tracker.lock().elapsed_ms(&self.params.run_id)
    }

    /// Take a run concurrency slot, reporting queue pressure and wait time.
    async fn acquire_agent_slot(&self) -> Result<tokio::sync::SemaphorePermit<'_>, HostError> {
        match self.agent_slots.try_acquire() {
            Ok(permit) if self.params.cancel.is_cancelled() => {
                drop(permit);
                Err(HostError::Cancelled)
            }
            Ok(permit) => Ok(permit),
            Err(tokio::sync::TryAcquireError::Closed) => {
                debug_assert!(false, "agent slot semaphore is never closed");
                Err(HostError::Cancelled)
            }
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                // During cancellation many spawns hit NoPermits at once; do not log that as queue pressure
                if self.params.cancel.is_cancelled() {
                    return Err(HostError::Cancelled);
                }
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::SubagentLimitHit::workflow_run_concurrent(
                        self.params.parent_session_id.clone(),
                        self.params.run_id.clone(),
                        self.params.max_concurrent_agents as u64,
                        // Slots in use; the active_agents counter lags spawn setup and is racy here
                        (self
                            .params
                            .max_concurrent_agents
                            .saturating_sub(self.agent_slots.available_permits()))
                            as u32,
                    ),
                );
                let wait_started_at = std::time::Instant::now();
                let permit = tokio::select! {
                    biased;
                    _ = self.params.cancel.cancelled() => return Err(HostError::Cancelled),
                    permit = self.agent_slots.acquire() => {
                        match permit {
                            Ok(permit) => permit,
                            Err(_closed) => {
                                debug_assert!(false, "agent slot semaphore is never closed");
                                return Err(HostError::Cancelled);
                            }
                        }
                    }
                };
                let waited_ms =
                    u64::try_from(wait_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                let stats = &self.params.stats;
                stats.slot_waits.fetch_add(1, Ordering::Relaxed);
                stats
                    .slot_wait_ms_total
                    .fetch_add(waited_ms, Ordering::Relaxed);
                stats
                    .slot_wait_ms_max
                    .fetch_max(waited_ms, Ordering::Relaxed);
                Ok(permit)
            }
        }
    }

    async fn spawn_agent(
        &self,
        mut opts: AgentOpts,
        ticket: Option<TicketHandle>,
    ) -> Result<AgentResult, HostError> {
        if self.params.cancel.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        if opts.prompt.len() > WORKFLOW_MAX_AGENT_PROMPT_BYTES {
            return Err(HostError::Failed(format!(
                "agent prompt exceeds {WORKFLOW_MAX_AGENT_PROMPT_BYTES} bytes"
            )));
        }
        if opts.fork_context && !self.params.allow_fork_context {
            return Err(HostError::Unsupported(
                "fork_context is restricted to built-in workflows".into(),
            ));
        }
        if opts.resume_from.is_some() {
            opts.fork_context = false;
        }

        if opts
            .label
            .as_ref()
            .is_some_and(|label| label.len() > WORKFLOW_MAX_PHASE_BYTES)
            || opts
                .phase
                .as_ref()
                .is_some_and(|phase| phase.len() > WORKFLOW_MAX_PHASE_BYTES)
        {
            return Err(HostError::Failed(
                "agent label and phase must each be at most 256 bytes".into(),
            ));
        }

        if opts.max_output_tokens.is_some() {
            tracing::debug!(
                run_id = %self.params.run_id,
                "agent max_output_tokens is deprecated and ignored; workflows budget logical agent calls"
            );
        }

        let reasoning_effort = opts
            .effort
            .as_deref()
            .map(|effort| {
                effort
                    .parse::<distill_sampling_types::ReasoningEffort>()
                    .map_err(|error| {
                        HostError::Failed(format!("invalid workflow agent effort: {error}"))
                    })
            })
            .transpose()?
            .or(self.params.effort);

        let id = uuid::Uuid::now_v7().to_string();
        let explicit_label = opts.label.clone();
        let capability_mode = match opts.capability_mode.as_deref() {
            None => None,
            Some(s) => Some(
                serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(|_| {
                    HostError::Failed(format!(
                        "invalid capability_mode '{s}' (expected read-only, read-write, \
                             execute, or all)"
                    ))
                })?,
            ),
        };
        let isolation = opts
            .isolation_worktree
            .then_some(distill_tool_types::SubagentIsolationMode::Worktree);
        let subagent_type = opts
            .agent_type
            .clone()
            .unwrap_or_else(|| "general-purpose".to_string());

        let schema_validator = match &opts.output_schema {
            None => None,
            Some(schema) => Some(compile_contract_schema(schema).map_err(HostError::Failed)?),
        };
        let prompt = match &opts.output_schema {
            None => opts.prompt.clone(),
            Some(schema) => contract_prompt(&opts.prompt, schema),
        };

        // The child's own token. For a ticket it is the one `cancel(ticket)` cancels, so the script can stop this
        // agent alone; `agent()` and `parallel()` get a token nothing else holds.
        let cancel_token = ticket.map(|ticket| ticket.cancel).unwrap_or_default();

        // Acquire before the roster row so a waiting agent is not shown as running
        let _agent_slot = tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                return Ok(AgentResult {
                    agent_id: id,
                    success: false,
                    output: serde_json::Value::String(
                        "cancelled by the workflow script before it started".into(),
                    ),
                    cancelled: true,
                    tokens_used: 0,
                    duration_ms: 0,
                });
            }
            slot = self.acquire_agent_slot() => slot?,
        };

        let description = self.params.tracker.lock().agent_started(
            &self.params.run_id,
            crate::session::workflow::tracker::WorkflowAgentRow {
                agent_id: id.clone(),
                label: explicit_label.unwrap_or_default(),
                phase: opts.phase.clone(),
                model: opts.model.clone(),
                state: "running".to_string(),
                tokens_used: 0,
                duration_ms: 0,
            },
        );
        let mut row = FinishOnce {
            host: self,
            agent_id: id.clone(),
            finished: false,
        };

        let spawn_once =
            |child_id: String, prompt: String, resume_from: Option<String>, fork_context: bool| {
                SubagentRequest {
                    jev_worker_ok: false,
                    id: child_id,
                    prompt,
                    description: description.clone(),
                    subagent_type: subagent_type.clone(),
                    parent_session_id: self.params.parent_session_id.clone(),
                    parent_prompt_id: None,
                    resume_from,
                    cwd: None,
                    runtime_overrides: SubagentRuntimeOverrides {
                        model: opts.model.clone(),
                        reasoning_effort: reasoning_effort.map(|effort| effort.to_string()),
                        output_token_budget: None,
                        model_override_provenance: ModelOverrideProvenance::Tool,
                        capability_mode,
                        isolation,
                        output_schema: None,
                        ..Default::default()
                    },
                    run_in_background: false,
                    surface_completion: false,
                    await_to_completion: true,
                    fork_context,
                    owner: SubagentOwner::workflow(&self.params.run_id),
                    cancel_token: cancel_token.clone(),
                    spawn_root: Default::default(),
                }
            };

        let mut attempts: u32 = 0;
        let mut total_tokens: u64 = 0;
        let mut total_duration: u64 = 0;
        let mut resume_child: Option<String> = opts.resume_from.clone();
        let mut next_prompt = prompt;
        let mut fork_context = opts.fork_context;

        let (result, output) = loop {
            attempts += 1;
            let run = self.agent_runs.fetch_add(1, Ordering::Relaxed);
            if run >= WORKFLOW_MAX_AGENT_RUNS {
                row.finish("failed", total_tokens, total_duration);
                return Err(HostError::Failed(format!(
                    "workflow agent-run quota exceeded (maximum {WORKFLOW_MAX_AGENT_RUNS})"
                )));
            }
            self.tick();
            let child_id = if attempts == 1 {
                id.clone()
            } else {
                let retry_id = uuid::Uuid::now_v7().to_string();
                row.rebind(&retry_id);
                retry_id
            };
            let request = spawn_once(
                child_id.clone(),
                next_prompt.clone(),
                resume_child,
                fork_context,
            );

            let now_running = self.active_agents.fetch_add(1, Ordering::Relaxed) + 1;
            self.params
                .stats
                .peak_concurrent
                .fetch_max(now_running, Ordering::Relaxed);
            self.tick();

            let backend = ChannelBackend::new(self.params.subagent_event_tx.clone());
            let result_fut = backend.spawn(request, None);
            tokio::pin!(result_fut);
            let result = tokio::select! {
                result = &mut result_fut => result,
                _ = self.params.cancel.cancelled() => {
                    cancel_token.cancel();
                    self.active_agents.fetch_sub(1, Ordering::Relaxed);
                    row.finish("cancelled", total_tokens, total_duration);
                    return Err(HostError::Cancelled);
                }
            };
            self.active_agents.fetch_sub(1, Ordering::Relaxed);

            let Ok(result) = result else {
                row.finish("failed", total_tokens, total_duration);
                return Err(HostError::Failed(
                    "subagent coordinator channel closed before completion".into(),
                ));
            };
            total_tokens = total_tokens.saturating_add(result.total_tokens_used);
            total_duration += result.duration_ms;
            if self.params.cancel.is_cancelled() {
                row.finish("cancelled", total_tokens, total_duration);
                return Err(HostError::Cancelled);
            }

            if result.backgrounded {
                row.finish("failed", total_tokens, total_duration);
                self.tick();
                return Err(HostError::Failed(format!(
                    "subagent {child_id} was auto-backgrounded by the await budget; its result \
                     is not available to this run (engine bug — workflow spawns must await to \
                     completion)"
                )));
            }

            let Some(validator) = schema_validator.as_ref() else {
                let output = if result.success {
                    serde_json::Value::String(result.output.to_string())
                } else {
                    serde_json::Value::String(
                        result
                            .error
                            .clone()
                            .unwrap_or_else(|| result.output.to_string()),
                    )
                };
                break (result, output);
            };
            if !result.success {
                let output = serde_json::Value::String(
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| result.output.to_string()),
                );
                break (result, output);
            }

            match validate_contract_output(validator, &result.output) {
                Ok(value) => break (result, value),
                Err(err) if attempts <= SCHEMA_CONTRACT_RETRIES => {
                    resume_child = Some(result.child_session_id.clone());
                    fork_context = false;
                    next_prompt = format!(
                        "Your final message did not satisfy the output contract: {err}\n\
                         Reply with a single ```json fenced block containing one JSON \
                         value conforming to the schema from <output-contract>, and \
                         nothing else."
                    );
                    continue;
                }
                Err(err) => {
                    let mut result = result;
                    result.success = false;
                    let output = serde_json::Value::String(format!(
                        "structured output validation failed: {err}"
                    ));
                    break (result, output);
                }
            }
        };

        row.finish(
            if result.success {
                "done"
            } else if result.cancelled && cancel_token.is_cancelled() {
                // The script's own `cancel(ticket)`, not a failure of the agent.
                "cancelled"
            } else {
                "failed"
            },
            total_tokens,
            total_duration,
        );
        self.tick();

        Ok(AgentResult {
            agent_id: id,
            success: result.success,
            output,
            cancelled: result.cancelled,
            tokens_used: total_tokens,
            duration_ms: total_duration,
        })
    }

    /// `sh()` / `spawn_sh()`: one shell command in the workflow's working directory, with no model involved.
    async fn run_command(
        &self,
        command: String,
        opts: ShOpts,
        ticket: Option<TicketHandle>,
    ) -> Result<ShResult, HostError> {
        if self.params.cancel.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        if command.len() > WORKFLOW_MAX_COMMAND_BYTES {
            return Err(HostError::Failed(format!(
                "sh command exceeds {WORKFLOW_MAX_COMMAND_BYTES} bytes"
            )));
        }
        if opts
            .label
            .as_ref()
            .is_some_and(|label| label.len() > WORKFLOW_MAX_PHASE_BYTES)
        {
            return Err(HostError::Failed(
                "sh label must be at most 256 bytes".into(),
            ));
        }
        let cwd = self.command_cwd(opts.cwd.as_deref())?;
        self.authorize_command(&command, &cwd).await?;

        let script_cancel = ticket.map(|ticket| ticket.cancel).unwrap_or_default();
        // Commands share the run's concurrency slots with agents, so a wide fan-out cannot fork without bound.
        let _slot = tokio::select! {
            biased;
            _ = script_cancel.cancelled() => return Ok(cancelled_command(0)),
            slot = self.acquire_agent_slot() => slot?,
        };

        let row_id = format!("sh-{}", uuid::Uuid::now_v7());
        let label = opts.label.clone().unwrap_or_else(|| {
            let first_line = command.lines().next().unwrap_or_default();
            format!("$ {}", distill_tools::util::truncate_str(first_line, 60))
        });
        self.params.tracker.lock().agent_started(
            &self.params.run_id,
            crate::session::workflow::tracker::WorkflowAgentRow {
                agent_id: row_id.clone(),
                label,
                phase: None,
                model: Some("sh".to_string()),
                state: "running".to_string(),
                tokens_used: 0,
                duration_ms: 0,
            },
        );
        self.tick();

        let timeout_ms = opts
            .timeout_ms
            .unwrap_or(DEFAULT_SH_TIMEOUT_MS)
            .min(WORKFLOW_MAX_COMMAND_TIMEOUT_MS);
        let started = std::time::Instant::now();
        let result = self
            .execute_command(&command, &cwd, timeout_ms, &script_cancel)
            .await;
        let (state, duration_ms) = match &result {
            Ok(result) if result.cancelled => ("cancelled", result.duration_ms),
            Ok(result) if result.ok => ("done", result.duration_ms),
            Ok(result) => ("failed", result.duration_ms),
            Err(HostError::Cancelled) => ("cancelled", started.elapsed().as_millis() as u64),
            Err(_) => ("failed", started.elapsed().as_millis() as u64),
        };
        self.params.tracker.lock().agent_finished(
            &self.params.run_id,
            &row_id,
            state,
            0,
            duration_ms,
        );
        self.tick();
        result
    }

    /// The directory a command runs in: the workflow's own, or a directory below it named by `cwd`.
    fn command_cwd(&self, cwd: Option<&str>) -> Result<PathBuf, HostError> {
        let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
            return Ok(self.params.cwd.clone());
        };
        let inside = Path::new(cwd).components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
        if !inside {
            return Err(HostError::Failed(format!(
                "sh cwd must be a relative path inside the workflow's working directory, got: {cwd}"
            )));
        }
        let dir = self.params.cwd.join(cwd);
        if !dir.is_dir() {
            return Err(HostError::Failed(format!(
                "sh cwd is not a directory: {cwd}"
            )));
        }
        Ok(dir)
    }

    /// Decides a workflow command the way the session decides an agent's terminal command, minus the human.
    ///
    /// A workflow runs unattended, so `sh()` is refused unless the session is in always-approve mode: anywhere else
    /// each command needs an approval nobody is there to give. In always-approve mode the command still goes to the
    /// permission manager, which enforces the configured deny rules before it approves (and shows its own prompt
    /// for a rule that says "ask"). Nothing here approves a command by itself.
    async fn authorize_command(&self, command: &str, cwd: &Path) -> Result<(), HostError> {
        let permissions = &self.params.permissions;
        if !permissions.is_yolo_mode() {
            return Err(HostError::Unsupported(
                "sh() runs a shell command with nobody there to approve it, so it needs the session in \
                 always-approve permission mode; this session asks before running commands. Switch the \
                 permission mode and resume, or have an agent run the command instead."
                    .into(),
            ));
        }
        let tool_call_update = agent_client_protocol::ToolCallUpdate::new(
            agent_client_protocol::ToolCallId::new(format!("workflow-sh-{}", uuid::Uuid::now_v7())),
            agent_client_protocol::ToolCallUpdateFields::new()
                .title(format!("Workflow command: {command}"))
                .kind(agent_client_protocol::ToolKind::Execute)
                .raw_input(serde_json::json!({ "command": command, "cwd": cwd })),
        );
        let request = PermissionRequest {
            path_context: Some(distill_workspace::permission::types::RequestPathContext {
                real_cwd: cwd.to_path_buf(),
                display_cwd: None,
            }),
            session_id: Some(self.params.parent_session_id.clone()),
            ..PermissionRequest::new(AccessKind::Bash(command.to_string()), tool_call_update)
        };
        let resolution = tokio::select! {
            biased;
            _ = self.params.cancel.cancelled() => return Err(HostError::Cancelled),
            resolution = permissions.request(request) => resolution,
        };
        match resolution.decision {
            Decision::Allow => Ok(()),
            Decision::PolicyDeny(reason) | Decision::Reject(reason) => Err(HostError::Unsupported(
                format!("sh() command was denied by the session's permission rules: {reason}"),
            )),
            Decision::Ask | Decision::FollowupMessage(_) | Decision::Cancelled => {
                Err(HostError::Unsupported(
                    "sh() command was not approved by the session's permission rules".into(),
                ))
            }
        }
    }

    async fn execute_command(
        &self,
        command: &str,
        cwd: &Path,
        timeout_ms: u64,
        script_cancel: &CancellationToken,
    ) -> Result<ShResult, HostError> {
        let mut cmd = shell_command(command);
        cmd.current_dir(cwd)
            .stdin(distill_tty_utils::null_stdio())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .envs(distill_tty_utils::pager_env())
            .kill_on_drop(true);
        // Own session and process group: no terminal to read from, and the whole tree can be killed at once.
        distill_tty_utils::detach_command(&mut cmd);
        distill_sandbox::child_net::restrict_child_network(&mut cmd);

        let started = std::time::Instant::now();
        // Enrolled on the next line; `ProcessScope::spawn` would `setpgid` first and defeat the `setsid` above.
        #[allow(clippy::disallowed_methods)]
        let mut child = cmd.spawn().map_err(|error| {
            HostError::Failed(format!("sh could not start the command: {error}"))
        })?;
        let group = distill_tty_utils::global_process_scope()
            .enroll(&child)
            .inspect_err(|error| {
                tracing::warn!(run_id = %self.params.run_id, %error, "workflow command has no process group; only its shell can be killed");
            })
            .ok();
        let (stdout, stdout_reader) = capture_tail(child.stdout.take());
        let (stderr, stderr_reader) = capture_tail(child.stderr.take());

        enum End {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            RunCancelled,
            ScriptCancelled,
        }
        let end = tokio::select! {
            biased;
            _ = self.params.cancel.cancelled() => End::RunCancelled,
            _ = script_cancel.cancelled() => End::ScriptCancelled,
            status = child.wait() => End::Exited(status),
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => End::TimedOut,
        };
        if !matches!(end, End::Exited(_)) {
            if let Some(group) = &group {
                let _ = group.kill();
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        let duration_ms = started.elapsed().as_millis() as u64;

        // The pipes close when the last process holding them exits. A grandchild left in the background would keep
        // them open for as long as it lives, so after a grace period the rest of the tree goes too.
        let abandon = [stdout_reader.abort_handle(), stderr_reader.abort_handle()];
        let mut readers = Box::pin(async {
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
        });
        if tokio::time::timeout(WORKFLOW_COMMAND_PIPE_DRAIN, &mut readers)
            .await
            .is_err()
        {
            if let Some(group) = &group {
                let _ = group.kill();
            }
            let _ = tokio::time::timeout(WORKFLOW_COMMAND_PIPE_DRAIN, &mut readers).await;
        }
        drop(readers);
        // A process that left the group can still hold a pipe; what was read so far is in the shared tails.
        for reader in abandon {
            reader.abort();
        }
        drop(group);

        let (stdout, stdout_truncated) = std::mem::take(&mut *stdout.lock()).into_text("stdout");
        let (stderr, stderr_truncated) = std::mem::take(&mut *stderr.lock()).into_text("stderr");
        let captured = ShResult {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            duration_ms,
            ..Default::default()
        };
        match end {
            End::RunCancelled => Err(HostError::Cancelled),
            End::ScriptCancelled => Ok(ShResult {
                cancelled: true,
                ..captured
            }),
            End::TimedOut => Ok(ShResult {
                timed_out: true,
                ..captured
            }),
            End::Exited(Ok(status)) => Ok(ShResult {
                ok: status.success(),
                code: status.code().map(i64::from),
                ..captured
            }),
            End::Exited(Err(error)) => Err(HostError::Failed(format!(
                "sh lost track of the command: {error}"
            ))),
        }
    }

    fn tick(&self) {
        let state = self.params.tracker.lock().get(&self.params.run_id);
        if let Some(state) = state {
            self.params.notify.emit_ephemeral(
                &state,
                self.elapsed_ms(),
                self.active_agents.load(Ordering::Relaxed),
            );
        }
    }

    async fn cancel_and_drain_children(&self) -> HostDrainOutcome {
        let (respond_to, response) = oneshot::channel();
        if self
            .params
            .subagent_event_tx
            .send(SubagentEvent::Cancel(SubagentCancelRequest {
                parent_session_id: Some(self.params.parent_session_id.clone()),
                target: SubagentCancelTarget::WorkflowRunId(self.params.run_id.clone()),
                respond_to,
            }))
            .is_err()
        {
            tracing::warn!(run_id = %self.params.run_id, "workflow child cancellation channel closed");
            return HostDrainOutcome::TimedOut;
        }
        if matches!(
            tokio::time::timeout(WORKFLOW_CHILD_DRAIN_TIMEOUT, response).await,
            Ok(Ok(_))
        ) {
            HostDrainOutcome::Drained
        } else {
            tracing::warn!(
                run_id = %self.params.run_id,
                timeout_ms = WORKFLOW_CHILD_DRAIN_TIMEOUT.as_millis() as u64,
                "workflow child cancel/drain timed out"
            );
            HostDrainOutcome::TimedOut
        }
    }

    fn render_template(&self, name: &str, vars: &serde_json::Value) -> Result<String, HostError> {
        let template = self
            .params
            .templates
            .get(name)
            .ok_or_else(|| HostError::Failed(format!("unknown template: {name}")))?;
        let mut out = template.clone();
        if out.len() > WORKFLOW_MAX_TEMPLATE_OUTPUT_BYTES {
            return Err(HostError::Failed(format!(
                "template exceeds {WORKFLOW_MAX_TEMPLATE_OUTPUT_BYTES} bytes"
            )));
        }
        if let Some(map) = vars.as_object() {
            for (key, value) in map {
                let value = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out = out.replace(&format!("{{{key}}}"), &value);
                if out.len() > WORKFLOW_MAX_TEMPLATE_OUTPUT_BYTES {
                    return Err(HostError::Failed(format!(
                        "rendered template exceeds {WORKFLOW_MAX_TEMPLATE_OUTPUT_BYTES} bytes"
                    )));
                }
            }
        }
        Ok(out)
    }

    fn scratch_paths(&self, name: &str) -> Result<(PathBuf, String), HostError> {
        if name.len() > WORKFLOW_MAX_SCRATCH_NAME_BYTES {
            return Err(HostError::Failed(format!(
                "scratch file name exceeds {WORKFLOW_MAX_SCRATCH_NAME_BYTES} bytes"
            )));
        }
        let candidate = Path::new(name);
        let mut components = candidate.components();
        match (components.next(), components.next()) {
            (Some(std::path::Component::Normal(part)), None) if !part.is_empty() => {}
            _ => {
                return Err(HostError::Failed(format!(
                    "scratch file name must be a single relative path component, got: {name}"
                )));
            }
        }
        Ok((
            self.params.scratch_dir.join(name),
            format!("{SCRATCH_ARTIFACT_ROOT}/{name}"),
        ))
    }

    fn reject_symlink(path: &Path, what: &str) -> Result<(), HostError> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(HostError::Failed(format!(
                "{what} must not be a symlink: {}",
                path.display()
            ))),
            Ok(meta) if what == "scratch directory" && !meta.is_dir() => {
                Err(HostError::Failed(format!(
                    "scratch directory is not a real directory: {}",
                    path.display()
                )))
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HostError::Failed(format!("{what} metadata: {e}"))),
        }
    }

    fn scratch_usage(&self, replacing: &Path) -> Result<(usize, u64), HostError> {
        let entries = match std::fs::read_dir(&self.params.scratch_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(e) => return Err(HostError::Failed(format!("scratch dir listing: {e}"))),
        };
        let mut files = 0usize;
        let mut bytes = 0u64;
        for entry in entries {
            let entry = entry.map_err(|e| HostError::Failed(format!("scratch dir entry: {e}")))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| HostError::Failed(format!("scratch metadata: {e}")))?;
            if meta.file_type().is_symlink() {
                return Err(HostError::Failed(format!(
                    "scratch directory contains a symlink: {}",
                    path.display()
                )));
            }
            if meta.is_file() && path != replacing {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(meta.len());
            }
        }
        Ok((files, bytes))
    }

    async fn write_scratch_file(&self, name: &str, content: &str) -> Result<String, HostError> {
        if content.len() > WORKFLOW_MAX_SCRATCH_FILE_BYTES {
            return Err(HostError::Failed(format!(
                "scratch file exceeds {WORKFLOW_MAX_SCRATCH_FILE_BYTES} byte limit"
            )));
        }
        let (path, artifact_path) = self.scratch_paths(name)?;
        let _io = self.scratch_io.lock().await;

        Self::reject_symlink(&self.params.scratch_dir, "scratch directory")?;
        tokio::fs::create_dir_all(&self.params.scratch_dir)
            .await
            .map_err(|e| HostError::Failed(format!("scratch dir: {e}")))?;
        Self::reject_symlink(&self.params.scratch_dir, "scratch directory")?;
        Self::reject_symlink(&path, "scratch file")?;

        let (other_files, other_bytes) = self.scratch_usage(&path)?;
        let target_exists = std::fs::symlink_metadata(&path)
            .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink());
        let resulting_files = other_files.saturating_add(usize::from(!target_exists));
        let resulting_bytes = other_bytes.saturating_add(content.len() as u64);
        if resulting_files > WORKFLOW_MAX_SCRATCH_FILES {
            return Err(HostError::Failed(format!(
                "scratch file quota exceeded (maximum {WORKFLOW_MAX_SCRATCH_FILES})"
            )));
        }
        if resulting_bytes > WORKFLOW_MAX_SCRATCH_TOTAL_BYTES {
            return Err(HostError::Failed(format!(
                "scratch byte quota exceeded (maximum {WORKFLOW_MAX_SCRATCH_TOTAL_BYTES})"
            )));
        }

        let scratch_dir = self.params.scratch_dir.clone();
        let body = content.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let mut tmp = tempfile::NamedTempFile::new_in(&scratch_dir)
                .map_err(|e| HostError::Failed(format!("scratch temp file: {e}")))?;
            Self::reject_symlink(&scratch_dir, "scratch directory")?;
            Self::reject_symlink(&path, "scratch file")?;
            tmp.write_all(&body)
                .map_err(|e| HostError::Failed(format!("scratch write: {e}")))?;
            tmp.persist(&path)
                .map_err(|e| HostError::Failed(format!("scratch atomic persist: {}", e.error)))?;
            Ok::<(), HostError>(())
        })
        .await
        .map_err(|e| HostError::Failed(format!("scratch writer task failed: {e}")))??;
        Ok(artifact_path)
    }

    async fn read_scratch_file(&self, name: &str) -> Result<String, HostError> {
        let (path, _) = self.scratch_paths(name)?;
        let _io = self.scratch_io.lock().await;
        Self::reject_symlink(&self.params.scratch_dir, "scratch directory")?;
        Self::reject_symlink(&path, "scratch file")?;
        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|e| HostError::Failed(format!("scratch read metadata: {e}")))?;
        if meta.file_type().is_symlink() {
            return Err(HostError::Failed(
                "scratch file must not be a symlink".into(),
            ));
        }
        if !meta.is_file() {
            return Err(HostError::Failed(
                "scratch path is not a regular file".into(),
            ));
        }
        if meta.len() > WORKFLOW_MAX_SCRATCH_FILE_BYTES as u64 {
            return Err(HostError::Failed(format!(
                "scratch file exceeds {WORKFLOW_MAX_SCRATCH_FILE_BYTES} byte read limit"
            )));
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| HostError::Failed(format!("scratch read: {e}")))?;
        String::from_utf8(bytes)
            .map_err(|e| HostError::Failed(format!("scratch file is not UTF-8: {e}")))
    }

    async fn git_diff_since(&self, commit: &str) -> Result<String, HostError> {
        const DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
        const DIFF_CAP_BYTES: usize = 256 * 1024;

        if commit.is_empty() || !commit.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(HostError::Failed(format!(
                "git_diff_since expects a commit hash, got: {commit}"
            )));
        }

        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("diff")
            .arg(commit)
            .current_dir(&self.params.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .envs(distill_tty_utils::pager_env());
        distill_tty_utils::detach_command(&mut cmd);

        let output = tokio::time::timeout(DIFF_TIMEOUT, cmd.output())
            .await
            .map_err(|_| HostError::Failed("git diff timed out".into()))?
            .map_err(|e| HostError::Failed(format!("git diff: {e}")))?;

        if !output.status.success() {
            return Err(HostError::Failed(format!(
                "git diff exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.len() > DIFF_CAP_BYTES {
            let mut end = DIFF_CAP_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("\n… [diff truncated]");
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::persistence::PersistenceMsg;
    use crate::session::workflow::notify::WorkflowNotifySender;
    use crate::session::workflow::store::WorkflowRunStore;
    use crate::session::workflow::tracker::WorkflowTracker;

    /// The returned receiver keeps the persistence channel open for the test's lifetime.
    fn test_host_params(
        run_id: &str,
        max_concurrent_agents: usize,
        scratch_suffix: &str,
        tracker: Arc<parking_lot::Mutex<WorkflowTracker>>,
        subagent_event_tx: mpsc::UnboundedSender<
            distill_tools::implementations::distill::task::types::SubagentEvent,
        >,
    ) -> (WorkflowHostParams, mpsc::UnboundedReceiver<PersistenceMsg>) {
        let (persist_tx, persist_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
        let store = WorkflowRunStore::new(None, persist_tx.clone());
        let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
        let notify = WorkflowNotifySender::new(
            agent_client_protocol::SessionId::new("test-session"),
            distill_acp_lib::AcpAgentGatewaySender::new(gateway_tx),
            persist_tx,
            store.clone(),
        );
        (
            WorkflowHostParams {
                run_id: run_id.to_owned(),
                max_concurrent_agents,
                cwd: std::env::temp_dir(),
                scratch_dir: std::env::temp_dir().join(scratch_suffix),
                tracker,
                store,
                notify,
                subagent_event_tx,
                parent_session_id: "parent".into(),
                permissions: PermissionHandle::allow_all(),
                allow_fork_context: false,
                effort: None,
                templates: Default::default(),
                telemetry: Arc::new(|_, _, _| {}),
                stats: Arc::new(WorkflowAgentStats::default()),
                cancel: CancellationToken::new(),
            },
            persist_rx,
        )
    }

    #[test]
    fn concurrent_agents_clamps_to_parallelism() {
        let from = workflow_max_concurrent_agents_from;
        assert_eq!(from(8, 64), 8);
        assert_eq!(from(64, 8), 8);
        assert_eq!(from(64, 1), 2);
        assert_eq!(from(0, 64), 1);
    }

    #[tokio::test]
    async fn reserve_agent_calls_rolls_back_on_persist_failure() {
        let run_id = "wf_reserve_rollback".to_string();
        let mut tracker = WorkflowTracker::default();
        tracker.start_run(
            run_id.clone(),
            "demo".into(),
            "objective".into(),
            vec![],
            Some(1000),
            None,
        );
        tracker.reserve_agents(&run_id, 10).unwrap();
        assert_eq!(tracker.get(&run_id).unwrap().agents_used, 10);

        let tracker = Arc::new(parking_lot::Mutex::new(tracker));
        let (subagent_tx, _subagent_rx) = mpsc::unbounded_channel();
        let (params, _persist_rx) = test_host_params(
            &run_id,
            2,
            "wf-scratch-reserve-rollback",
            tracker.clone(),
            subagent_tx,
        );
        let cancel = params.cancel.clone();

        let (host_tx, host_rx) = mpsc::unbounded_channel();
        let (handle, _drained) = spawn_workflow_host_service(params, host_rx);

        let (reply_tx, reply_rx) = oneshot::channel();
        host_tx
            .send(WorkflowHostRequest::ReserveAgentCalls {
                count: 40,
                reply: reply_tx,
            })
            .unwrap();

        let result = reply_rx.await.unwrap();
        assert!(
            matches!(
                result,
                Err(HostError::Failed(ref msg)) if msg.contains("could not be persisted")
            ),
            "expected persist failure, got {result:?}"
        );
        assert_eq!(
            tracker.lock().get(&run_id).unwrap().agents_used,
            10,
            "reserve must release_agents(count) when persist_now fails"
        );

        drop(host_tx);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn release_agent_calls_keeps_in_memory_release_on_persist_failure() {
        let run_id = "wf_release_persist".to_string();
        let mut tracker = WorkflowTracker::default();
        tracker.start_run(
            run_id.clone(),
            "demo".into(),
            "objective".into(),
            vec![],
            Some(1000),
            None,
        );
        tracker.reserve_agents(&run_id, 50).unwrap();
        assert_eq!(tracker.get(&run_id).unwrap().agents_used, 50);

        let tracker = Arc::new(parking_lot::Mutex::new(tracker));
        let (subagent_tx, _subagent_rx) = mpsc::unbounded_channel();
        let (params, _persist_rx) = test_host_params(
            &run_id,
            2,
            "wf-scratch-release-persist",
            tracker.clone(),
            subagent_tx,
        );
        let cancel = params.cancel.clone();

        let (host_tx, host_rx) = mpsc::unbounded_channel();
        let (handle, _drained) = spawn_workflow_host_service(params, host_rx);

        let (reply_tx, reply_rx) = oneshot::channel();
        host_tx
            .send(WorkflowHostRequest::ReleaseAgentCalls {
                count: 50,
                reply: reply_tx,
            })
            .unwrap();

        let result = reply_rx.await.unwrap();
        assert!(
            result.is_ok(),
            "release must succeed for accounting even if persist lags: {result:?}"
        );
        assert_eq!(
            tracker.lock().get(&run_id).unwrap().agents_used,
            0,
            "in-memory release must stick so resume does not double-charge"
        );

        drop(host_tx);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn a_run_keeps_at_most_max_concurrent_agents_live() {
        let run_id = "wf_agent_slots".to_string();
        let mut tracker = WorkflowTracker::default();
        tracker.start_run(
            run_id.clone(),
            "demo".into(),
            "objective".into(),
            vec![],
            Some(1000),
            None,
        );
        let tracker = Arc::new(parking_lot::Mutex::new(tracker));
        let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel();
        let (params, _persist_rx) = test_host_params(
            &run_id,
            1,
            "wf-scratch-agent-slots",
            tracker.clone(),
            subagent_tx,
        );
        let cancel = params.cancel.clone();
        let (host_tx, host_rx) = mpsc::unbounded_channel();
        let (handle, _drained) = spawn_workflow_host_service(params, host_rx);

        let spawn_agent = |prompt: &str| {
            let (reply_tx, reply_rx) = oneshot::channel();
            host_tx
                .send(WorkflowHostRequest::SpawnAgent {
                    opts: AgentOpts {
                        prompt: prompt.to_owned(),
                        ..Default::default()
                    },
                    ticket: None,
                    reply: reply_tx,
                })
                .unwrap();
            reply_rx
        };
        let succeed =
            |spawn: distill_tools::implementations::distill::task::types::SubagentSpawnRequest| {
                spawn
                    .respond_with(|request| {
                        distill_tools::implementations::distill::task::types::SubagentResult {
                            success: true,
                            output: Arc::from("done"),
                            subagent_id: request.id.clone(),
                            child_session_id: request.id.clone(),
                            ..Default::default()
                        }
                    })
                    .expect("agent result delivered");
            };

        let first = spawn_agent("first");
        let second = spawn_agent("second");

        let running = tokio::time::timeout(Duration::from_secs(5), subagent_rx.recv())
            .await
            .expect("an agent reaches the coordinator")
            .expect("subagent channel open");
        let SubagentEvent::Spawn(running) = running else {
            panic!("expected a spawn event");
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(100), subagent_rx.recv())
                .await
                .is_err(),
            "the second agent must wait for the run's only slot"
        );

        succeed(running);
        let queued = tokio::time::timeout(Duration::from_secs(5), subagent_rx.recv())
            .await
            .expect("the queued agent starts once the slot frees")
            .expect("subagent channel open");
        let SubagentEvent::Spawn(queued) = queued else {
            panic!("expected a spawn event");
        };
        succeed(queued);

        // Slot acquisition order between the two dispatched requests is unspecified, so assert both replies only after both agents ran
        assert!(
            first
                .await
                .expect("first reply")
                .expect("first result")
                .success,
            "first agent completes"
        );
        assert!(
            second
                .await
                .expect("second reply")
                .expect("second result")
                .success,
            "second agent completes"
        );

        drop(host_tx);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// A running host service for one test, with a real working directory for commands.
    struct TestHost {
        host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
        handle: tokio::task::JoinHandle<()>,
        cancel: CancellationToken,
        tracker: Arc<parking_lot::Mutex<WorkflowTracker>>,
        subagent_rx: mpsc::UnboundedReceiver<SubagentEvent>,
        dir: tempfile::TempDir,
        _persist_rx: mpsc::UnboundedReceiver<PersistenceMsg>,
    }

    impl TestHost {
        fn start(permissions: PermissionHandle, max_concurrent_agents: usize) -> Self {
            let run_id = "wf_commands".to_string();
            let mut tracker = WorkflowTracker::default();
            tracker.start_run(
                run_id.clone(),
                "demo".into(),
                "objective".into(),
                vec![],
                Some(1000),
                None,
            );
            let tracker = Arc::new(parking_lot::Mutex::new(tracker));
            let (subagent_tx, subagent_rx) = mpsc::unbounded_channel();
            let (mut params, persist_rx) = test_host_params(
                &run_id,
                max_concurrent_agents,
                "wf-scratch-commands",
                tracker.clone(),
                subagent_tx,
            );
            let dir = tempfile::tempdir().expect("temp working directory");
            params.cwd = dir.path().to_path_buf();
            params.permissions = permissions;
            let cancel = params.cancel.clone();
            let (host_tx, host_rx) = mpsc::unbounded_channel();
            let (handle, _drained) = spawn_workflow_host_service(params, host_rx);
            Self {
                host_tx,
                handle,
                cancel,
                tracker,
                subagent_rx,
                dir,
                _persist_rx: persist_rx,
            }
        }

        fn send_sh(
            &self,
            command: &str,
            opts: ShOpts,
            ticket: Option<TicketHandle>,
        ) -> oneshot::Receiver<Result<ShResult, HostError>> {
            let (reply, result) = oneshot::channel();
            self.host_tx
                .send(WorkflowHostRequest::RunCommand {
                    command: command.to_owned(),
                    opts,
                    ticket,
                    reply,
                })
                .expect("host service running");
            result
        }

        async fn sh(&self, command: &str, opts: ShOpts) -> Result<ShResult, HostError> {
            tokio::time::timeout(Duration::from_secs(30), self.send_sh(command, opts, None))
                .await
                .expect("the command replies")
                .expect("the host keeps the reply")
        }

        async fn stop(self) {
            drop(self.host_tx);
            self.cancel.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(2), self.handle).await;
        }
    }

    /// A permission manager that is in `yolo` (always-approve) mode or not and answers every request with `decide`,
    /// recording what it was asked.
    fn permission_manager(
        yolo: bool,
        decide: impl Fn() -> Decision + Send + 'static,
    ) -> (PermissionHandle, Arc<parking_lot::Mutex<Vec<AccessKind>>>) {
        use std::sync::atomic::{AtomicBool, AtomicUsize};

        use distill_workspace::permission::{PermissionCommand, PermissionResolution};

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let asked = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let asked_in_manager = asked.clone();
        tokio::spawn(async move {
            while let Some(command) = cmd_rx.recv().await {
                if let PermissionCommand::Request {
                    request,
                    respond_to,
                } = command
                {
                    asked_in_manager.lock().push(request.access);
                    let _ = respond_to.send(PermissionResolution {
                        decision: decide(),
                        event: None,
                    });
                }
            }
        });
        let handle = PermissionHandle::Actor {
            cmd_tx,
            yolo_state: Arc::new(AtomicBool::new(yolo)),
            auto_state: Arc::new(AtomicBool::new(false)),
            side_query_wired: Arc::new(AtomicBool::new(false)),
            yolo_pin: None,
            deny_read_globs: Arc::new(vec![]),
            in_flight: Arc::new(AtomicUsize::new(0)),
            user_prompt_notify: Arc::new(parking_lot::Mutex::new(None)),
        };
        (handle, asked)
    }

    fn ticket(id: &str) -> TicketHandle {
        TicketHandle {
            id: id.to_owned(),
            cancel: CancellationToken::new(),
        }
    }

    /// `sh()` replaces an agent whose only job was to run one command, so the script has to get back everything that
    /// agent would have reported: where it ran, the exit code, and both streams. A failing command is an answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn sh_runs_in_the_working_directory_and_returns_code_and_both_streams() {
        let host = TestHost::start(PermissionHandle::allow_all(), 2);
        std::fs::create_dir(host.dir.path().join("tools")).unwrap();

        let result = host
            .sh(
                "pwd; printf 'to stderr' >&2; exit 3",
                ShOpts {
                    cwd: Some("tools".into()),
                    label: Some("check".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("a failing command is a result, not a host error");
        assert!(!result.ok);
        assert_eq!(result.code, Some(3));
        assert!(
            result.stdout.trim_end().ends_with("/tools"),
            "ran in {:?}",
            result.stdout
        );
        assert_eq!(result.stderr, "to stderr");
        assert!(!result.timed_out && !result.cancelled && !result.stdout_truncated);

        let rows = host.tracker.lock().get("wf_commands").unwrap().agents;
        let row = rows.first().expect("the command shows in the run's roster");
        assert_eq!(
            (row.label.as_str(), row.model.as_deref(), row.state.as_str()),
            ("check", Some("sh"), "failed")
        );

        let outside = host
            .sh(
                "true",
                ShOpts {
                    cwd: Some("../elsewhere".into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(outside, Err(HostError::Failed(ref message)) if message.contains("inside")),
            "cwd is relative to the workflow's directory and may not leave it: {outside:?}"
        );
        host.stop().await;
    }

    /// The safety property of `sh()`: a workflow is unattended, so a session that would ask before running a command
    /// must refuse it rather than run it unapproved. The refusal has to be the catchable kind, and the command must
    /// not have started.
    #[cfg(unix)]
    #[tokio::test]
    async fn sh_is_refused_outside_always_approve_mode_and_does_not_run() {
        let (permissions, asked) = permission_manager(false, || Decision::Allow);
        let host = TestHost::start(permissions, 2);

        let result = host.sh("touch ran.marker", ShOpts::default()).await;
        assert!(
            matches!(result, Err(HostError::Unsupported(ref message)) if message.contains("always-approve")),
            "expected a refusal naming the mode, got {result:?}"
        );
        assert!(
            !host.dir.path().join("ran.marker").exists(),
            "a refused command must not run"
        );
        assert!(
            asked.lock().is_empty(),
            "outside always-approve the manager is not even asked: asking would raise a prompt nobody answers"
        );
        host.stop().await;
    }

    /// Always-approve is not "no rules": deny rules still bind an agent's terminal there, so they must bind `sh()`
    /// too. The command goes to the permission manager verbatim and only runs on its allow.
    #[cfg(unix)]
    #[tokio::test]
    async fn sh_in_always_approve_still_obeys_the_permission_manager() {
        let (permissions, asked) = permission_manager(true, || {
            Decision::PolicyDeny("Bash(touch:*) is denied".into())
        });
        let host = TestHost::start(permissions, 2);

        let result = host.sh("touch denied.marker", ShOpts::default()).await;
        assert!(
            matches!(result, Err(HostError::Unsupported(ref message)) if message.contains("Bash(touch:*) is denied")),
            "expected the rule's reason, got {result:?}"
        );
        assert!(!host.dir.path().join("denied.marker").exists());
        assert!(
            matches!(asked.lock().as_slice(), [AccessKind::Bash(command)] if command == "touch denied.marker"),
            "the manager decides on the exact command: {:?}",
            asked.lock()
        );
        host.stop().await;

        let (permissions, asked) = permission_manager(true, || Decision::Allow);
        let host = TestHost::start(permissions, 2);
        let result = host.sh("touch allowed.marker", ShOpts::default()).await;
        assert!(result.is_ok_and(|result| result.ok));
        assert!(host.dir.path().join("allowed.marker").exists());
        assert_eq!(asked.lock().len(), 1);
        host.stop().await;
    }

    #[cfg(unix)]
    fn process_is_gone(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the pid exists.
        unsafe { libc::kill(pid, 0) != 0 }
    }

    #[cfg(unix)]
    async fn wait_until_gone(pid: i32) -> bool {
        for _ in 0..100 {
            if process_is_gone(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// A hung command must not hang the workflow, and killing only the shell would leave what it started running
    /// (`bun`, a dev server) for the rest of the session. The timeout is a result the script can read.
    #[cfg(unix)]
    #[tokio::test]
    async fn sh_timeout_kills_the_whole_command_tree() {
        let host = TestHost::start(PermissionHandle::allow_all(), 2);
        let result = host
            .sh(
                "sleep 60 & echo $!; wait",
                ShOpts {
                    timeout_ms: Some(300),
                    ..Default::default()
                },
            )
            .await
            .expect("a timeout is a result");
        assert!(result.timed_out && !result.ok);
        assert_eq!(result.code, None);
        assert!(
            result.duration_ms < 20_000,
            "took {} ms",
            result.duration_ms
        );
        let grandchild: i32 = result.stdout.trim().parse().expect("the sleep's pid");
        assert!(
            wait_until_gone(grandchild).await,
            "the background child outlived the timeout"
        );
        host.stop().await;
    }

    /// Command output is unbounded and the result is journaled and usually pasted into a prompt, so only the end
    /// is kept (where a build or a test run says what failed) and the cut is announced.
    #[cfg(unix)]
    #[tokio::test]
    async fn sh_keeps_the_tail_of_long_output_and_says_it_was_cut() {
        let host = TestHost::start(PermissionHandle::allow_all(), 2);
        let result = host
            .sh(
                "head -c 300000 /dev/zero | tr '\\0' 'x'; echo; echo LAST LINE",
                ShOpts::default(),
            )
            .await
            .expect("result");
        assert!(result.ok);
        assert!(result.stdout_truncated && !result.stderr_truncated);
        assert!(
            result.stdout.ends_with("LAST LINE\n"),
            "the end of the output is what is kept"
        );
        assert!(
            result
                .stdout
                .starts_with("[stdout truncated: showing the last 65536 of 300011 bytes]\n"),
            "{:?}",
            result.stdout.lines().next()
        );
        assert!(result.stdout.len() < MAX_SH_OUTPUT_BYTES + 128);
        host.stop().await;
    }

    /// Pausing or stopping a run must not leave its commands running, and the script must see the run-level
    /// cancellation (which leaves the call unjournaled, so a resume runs it again), not a made-up result.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_the_run_kills_a_running_command() {
        let host = TestHost::start(PermissionHandle::allow_all(), 2);
        let pid_file = host.dir.path().join("pid");
        let pending = host.send_sh("echo $$ > pid; sleep 60", ShOpts::default(), None);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        host.cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), pending)
            .await
            .expect("the cancelled command replies")
            .expect("reply kept");
        assert!(matches!(result, Err(HostError::Cancelled)), "{result:?}");
        assert!(wait_until_gone(pid).await, "the command outlived the run");
        host.stop().await;
    }

    /// `cancel(ticket)` stops one command and nothing else: the run goes on, and the ticket still resolves (marked
    /// cancelled) so the script's wait returns.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_ticket_kills_only_its_command() {
        let host = TestHost::start(PermissionHandle::allow_all(), 2);
        let pid_file = host.dir.path().join("pid");
        let cancelled = ticket("t1");
        let pending = host.send_sh(
            "echo $$ > pid; sleep 60",
            ShOpts::default(),
            Some(cancelled.clone()),
        );
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        cancelled.cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), pending)
            .await
            .expect("the cancelled ticket replies")
            .expect("reply kept")
            .expect("a cancelled ticket is a result, not an error");
        assert!(result.cancelled && !result.ok && !result.timed_out);
        assert!(wait_until_gone(pid).await);

        let other = host.sh("echo still running", ShOpts::default()).await;
        assert_eq!(
            other.ok().map(|result| result.stdout).as_deref(),
            Some("still running\n"),
            "the run itself was not cancelled"
        );
        let states: Vec<String> = host
            .tracker
            .lock()
            .get("wf_commands")
            .unwrap()
            .agents
            .iter()
            .map(|row| row.state.clone())
            .collect();
        assert_eq!(states, ["cancelled", "done"]);
        host.stop().await;
    }

    /// The engine has no clock: `wait_any(tickets, timeout_ms)` times out only when the host says so. It must say
    /// so after the delay, not before, or a quorum gives up on agents that were about to answer.
    #[tokio::test(start_paused = true)]
    async fn wait_timeout_fires_after_its_delay() {
        let host = TestHost::start(PermissionHandle::allow_all(), 2);
        let (fire, mut fired) = oneshot::channel();
        host.host_tx
            .send(WorkflowHostRequest::AwaitTickets {
                tickets: vec!["t1".into()],
                all: false,
                timeout: Some(WaitTimeout { ms: 5_000, fire }),
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(4_900), &mut fired)
                .await
                .is_err(),
            "the timeout fired early"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(200), &mut fired).await,
            Ok(Ok(())),
            "the timeout did not fire at its deadline"
        );
        host.stop().await;
    }

    /// An agent waiting for a concurrency slot has not started. Cancelling its ticket must resolve it at once,
    /// without it ever reaching the coordinator, or a quorum's stragglers would still run later and be paid for.
    #[tokio::test]
    async fn cancelled_ticket_waiting_for_a_slot_never_starts_its_agent() {
        let mut host = TestHost::start(PermissionHandle::allow_all(), 1);
        let spawn = |prompt: &str, ticket: Option<TicketHandle>| {
            let (reply, result) = oneshot::channel();
            host.host_tx
                .send(WorkflowHostRequest::SpawnAgent {
                    opts: AgentOpts {
                        prompt: prompt.to_owned(),
                        ..Default::default()
                    },
                    ticket,
                    reply,
                })
                .unwrap();
            result
        };

        let running = spawn("holds the only slot", None);
        let holder = tokio::time::timeout(Duration::from_secs(5), host.subagent_rx.recv())
            .await
            .expect("the first agent reaches the coordinator")
            .expect("subagent channel open");
        let queued_ticket = ticket("t1");
        let queued = spawn("queued", Some(queued_ticket.clone()));
        queued_ticket.cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .expect("the cancelled ticket resolves while the slot is still taken")
            .expect("reply kept")
            .expect("a cancelled ticket is a result");
        assert!(result.cancelled && !result.success);
        assert!(
            host.subagent_rx.try_recv().is_err(),
            "the cancelled agent must not have been started"
        );

        let SubagentEvent::Spawn(holder) = holder else {
            panic!("expected a spawn event");
        };
        holder
            .respond_with(|request| {
                distill_tools::implementations::distill::task::types::SubagentResult {
                    success: true,
                    output: Arc::from("done"),
                    subagent_id: request.id.clone(),
                    child_session_id: request.id.clone(),
                    ..Default::default()
                }
            })
            .expect("agent result delivered");
        assert!(running.await.expect("reply").expect("result").success);
        host.stop().await;
    }
}
