// Modified for Distill by Samuel Fajreldines, 2026.
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use rhai::{Dynamic, EvalAltResult, Position};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::host::{
    AgentOpts, AgentResult, HostError, ShOpts, ShResult, TicketHandle, WaitTimeout,
    WorkflowHostRequest,
};
use crate::journal::{
    HOST_ERROR_KEY, Journal, JournalError, TICKET_AGENT_KIND, TICKET_SH_KIND, request_hash,
};
use crate::run::{PauseKind, WorkflowOutcome};
use crate::{MAX_HOST_CALLS, MAX_PARALLEL};

/// Milliseconds on a clock that only moves forward. `wait_any()` reads it to tell the script how long it was blocked.
pub type WorkflowClock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub struct WorkflowRunParams {
    pub script: String,
    pub args: serde_json::Value,
    pub journal: Journal,
    pub host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
    pub cancel: CancellationToken,
    pub max_ops: u64,
    /// `None` is the wall clock. A simulated host passes its virtual clock, so the durations a script sees agree
    /// with the timeouts that host fires.
    pub clock: Option<WorkflowClock>,
}

impl WorkflowRunParams {
    pub const DEFAULT_MAX_OPS: u64 = 100_000_000;
}

#[derive(Debug, Clone)]
enum ControlToken {
    Complete(serde_json::Value),
    Pause(PauseKind, String),
    Budget(String),
    Cancelled,
    Fatal(String),
}

struct Ctx {
    host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
    journal: Journal,
    /// Journal position of the next script call. Ticket completions share the journal, so this is a cursor, not
    /// a call count: it steps over them (see [`Ctx::skip_ticket_entries`]).
    seq: u64,
    /// Every `spawn_agent()` / `spawn_sh()` of this run, in call order; ticket `t<n>` is `tickets[n - 1]`.
    tickets: Vec<Ticket>,
    /// Ticket id to the journal position of its completion, for the tickets that finished in an earlier run.
    recorded_tickets: HashMap<String, u64>,
    clock: WorkflowClock,
}

impl Ctx {
    /// Whether the next script call will be answered from the journal.
    fn replaying(&self) -> bool {
        let mut seq = self.seq;
        while self.journal.is_ticket_entry(seq) {
            seq = seq.saturating_add(1);
        }
        self.journal.covers(seq)
    }

    /// Moves the cursor past ticket completions to the next call entry (or the journal's end). Call entries are
    /// matched in script order; a completion is found by its ticket id, wherever it sits.
    fn skip_ticket_entries(&mut self) {
        while self.journal.is_ticket_entry(self.seq) {
            self.seq = self.seq.saturating_add(1);
        }
    }

    fn next_seq(&mut self) -> ScriptResult<u64> {
        Ok(self.reserve_seqs(1)?.start)
    }

    fn reserve_seqs(&mut self, count: usize) -> ScriptResult<std::ops::Range<u64>> {
        self.skip_ticket_entries();
        let count = u64::try_from(count).map_err(|_| {
            terminated(ControlToken::Fatal(
                "workflow host-call count overflowed".into(),
            ))
        })?;
        let end = self.seq.checked_add(count).ok_or_else(|| {
            terminated(ControlToken::Fatal(
                "workflow host-call count overflowed".into(),
            ))
        })?;
        if end > MAX_HOST_CALLS {
            return Err(terminated(ControlToken::Fatal(format!(
                "workflow exceeded the maximum of {MAX_HOST_CALLS} result-bearing host calls"
            ))));
        }
        let start = self.seq;
        self.seq = end;
        Ok(start..end)
    }

    fn record(
        &mut self,
        seq: u64,
        kind: &str,
        hash: String,
        value: serde_json::Value,
    ) -> ScriptResult<()> {
        self.journal
            .record(seq, kind, hash, value)
            .map_err(journal_fatal)
    }

    /// Journals the completion of `tickets[index]` at the journal's end, the moment the script thread sees it. A
    /// ticket that cannot be journaled is lost for this run, exactly as if the host had cancelled it.
    fn record_ticket(&mut self, index: usize, value: serde_json::Value) -> ScriptResult<()> {
        let order = self.journal.len() as u64;
        let Some(ticket) = self.tickets.get_mut(index) else {
            return Ok(());
        };
        if order >= MAX_HOST_CALLS {
            ticket.state = TicketState::Lost;
            return Err(terminated(ControlToken::Fatal(format!(
                "workflow exceeded the maximum of {MAX_HOST_CALLS} result-bearing host calls"
            ))));
        }
        if let Err(error) = self.journal.record(
            order,
            ticket.kind.journal_kind(),
            ticket.hash.clone(),
            value.clone(),
        ) {
            ticket.state = TicketState::Lost;
            return Err(journal_fatal(error));
        }
        ticket.state = TicketState::Done { value };
        if self.seq == order {
            self.seq = order.saturating_add(1);
        }
        Ok(())
    }

    fn has_live_ticket(&self) -> bool {
        self.tickets
            .iter()
            .any(|ticket| matches!(ticket.state, TicketState::Live { .. }))
    }
}

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;

enum PendingAgent {
    Replayed(serde_json::Value),
    Live {
        seq: u64,
        hash: String,
        reply_rx: oneshot::Receiver<Result<crate::host::AgentResult, HostError>>,
    },
}

fn drain_parallel_replies(pending: Vec<PendingAgent>) {
    for entry in pending {
        if let PendingAgent::Live { reply_rx, .. } = entry {
            let _ = reply_rx.blocking_recv();
        }
    }
}

pub fn run_workflow(params: WorkflowRunParams) -> WorkflowOutcome {
    let WorkflowRunParams {
        script,
        args,
        journal,
        host_tx,
        cancel,
        max_ops,
        clock,
    } = params;

    let recorded_tickets = match recorded_tickets(&journal) {
        Ok(recorded) => recorded,
        Err(error) => return WorkflowOutcome::Failed { error },
    };
    let ctx = Rc::new(RefCell::new(Ctx {
        host_tx,
        journal,
        seq: 0,
        tickets: Vec::new(),
        recorded_tickets,
        clock: clock.unwrap_or_else(|| {
            let started = std::time::Instant::now();
            Arc::new(move || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
        }),
    }));

    let mut engine = rhai::Engine::new();
    engine.set_max_operations(max_ops);
    engine.set_max_call_levels(64);
    engine.set_max_expr_depths(128, 64);
    engine.set_max_string_size(16 * 1024 * 1024);
    engine.set_max_array_size(65_536);
    engine.set_max_map_size(65_536);
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    engine.disable_symbol("eval");
    engine.register_fn("timestamp", || -> ScriptResult<()> {
        Err(runtime_error(
            "timestamp() is unavailable: workflow scripts must be deterministic (wall-clock \
             time breaks resume). Pass timestamps in via `args` instead.",
        ))
    });
    engine.register_fn("sleep", |_seconds: i64| -> ScriptResult<()> {
        Err(runtime_error(
            "sleep() is unavailable in workflow scripts — host calls already block until \
             their work finishes.",
        ))
    });
    engine.register_fn("sleep", |_seconds: f64| -> ScriptResult<()> {
        Err(runtime_error(
            "sleep() is unavailable in workflow scripts — host calls already block until \
             their work finishes.",
        ))
    });
    engine.register_fn("exit", || -> ScriptResult<()> {
        Err(runtime_error(
            "exit() is unavailable — end a workflow with complete(value) or pause(kind, msg).",
        ))
    });

    engine.on_progress(move |_ops| {
        if cancel.is_cancelled() {
            Some(Dynamic::from(ControlToken::Cancelled))
        } else {
            None
        }
    });

    register_host_fns(&mut engine, &ctx);

    let ast = match engine.compile(&script) {
        Ok(ast) => ast,
        Err(e) => {
            return WorkflowOutcome::Failed {
                error: format!("script failed to compile: {e}"),
            };
        }
    };

    let mut scope = rhai::Scope::new();
    let args_dyn = match rhai::serde::to_dynamic(&args) {
        Ok(d) => d,
        Err(e) => {
            return WorkflowOutcome::Failed {
                error: format!("invalid workflow args: {e}"),
            };
        }
    };
    scope.push_dynamic("args", args_dyn);

    let outcome = match engine.eval_ast_with_scope::<Dynamic>(&mut scope, &ast) {
        Ok(value) => WorkflowOutcome::Completed {
            result: dynamic_to_value(value),
        },
        Err(err) => outcome_from_error(*err),
    };
    drain_tickets(&ctx, &outcome);
    outcome
}

fn recorded_tickets(journal: &Journal) -> Result<HashMap<String, u64>, String> {
    let mut recorded = HashMap::new();
    for entry in journal.ticket_entries() {
        let Some(id) = entry.result.get("ticket").and_then(|id| id.as_str()) else {
            return Err(format!(
                "workflow journal entry {} records a ticket without an id",
                entry.seq
            ));
        };
        if recorded.insert(id.to_string(), entry.seq).is_some() {
            return Err(format!("workflow journal records ticket {id} twice"));
        }
    }
    Ok(recorded)
}

fn outcome_from_error(err: EvalAltResult) -> WorkflowOutcome {
    if let Some(token) = find_control_token(&err) {
        return match token {
            ControlToken::Complete(result) => WorkflowOutcome::Completed { result },
            ControlToken::Pause(kind, message) => WorkflowOutcome::Paused { kind, message },
            ControlToken::Budget(message) => WorkflowOutcome::BudgetExceeded { message },
            ControlToken::Cancelled => WorkflowOutcome::Cancelled,
            ControlToken::Fatal(error) => WorkflowOutcome::Failed { error },
        };
    }
    WorkflowOutcome::Failed {
        error: crate::with_rhai_hint(err.to_string()),
    }
}

fn find_control_token(err: &EvalAltResult) -> Option<ControlToken> {
    match err {
        EvalAltResult::ErrorTerminated(token, _) => token.clone().try_cast::<ControlToken>(),
        EvalAltResult::ErrorInFunctionCall(_, _, inner, _) => find_control_token(inner),
        EvalAltResult::ErrorInModule(_, inner, _) => find_control_token(inner),
        _ => None,
    }
}

fn terminated(token: ControlToken) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorTerminated(
        Dynamic::from(token),
        Position::NONE,
    ))
}

fn runtime_error(message: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from(message.into()),
        Position::NONE,
    ))
}

fn journal_fatal(error: JournalError) -> Box<EvalAltResult> {
    terminated(ControlToken::Fatal(error.to_string()))
}

fn dynamic_to_value(d: Dynamic) -> serde_json::Value {
    rhai::serde::from_dynamic::<serde_json::Value>(&d).unwrap_or(serde_json::Value::Null)
}

fn value_to_dynamic(v: &serde_json::Value) -> ScriptResult<Dynamic> {
    rhai::serde::to_dynamic(v).map_err(|e| runtime_error(format!("host result conversion: {e}")))
}

fn map_to_value(map: rhai::Map) -> ScriptResult<serde_json::Value> {
    rhai::serde::from_dynamic::<serde_json::Value>(&Dynamic::from_map(map))
        .map_err(|e| runtime_error(format!("invalid options map: {e}")))
}

fn replay_spawn_agent(
    journal: &Journal,
    seq: u64,
    payload: &serde_json::Value,
    hash: &str,
) -> Result<Option<serde_json::Value>, JournalError> {
    let normal = journal.replay(seq, "spawn_agent", hash);
    match &normal {
        Ok(Some(_)) => return normal,
        Ok(None) | Err(JournalError::Divergence { .. }) => {}
        Err(_) => return normal,
    }

    let Some(mut legacy_payload) = payload.as_object().cloned() else {
        return normal;
    };
    if legacy_payload.remove("effort").is_none() {
        return normal;
    }
    let legacy_hash = request_hash("spawn_agent", &serde_json::Value::Object(legacy_payload));
    journal.replay(seq, "spawn_agent", &legacy_hash)
}

fn host_call<T>(
    ctx: &Rc<RefCell<Ctx>>,
    kind: &'static str,
    payload: serde_json::Value,
    build: impl FnOnce(oneshot::Sender<Result<T, HostError>>) -> WorkflowHostRequest,
    to_result: impl FnOnce(T) -> serde_json::Value,
) -> ScriptResult<serde_json::Value> {
    let hash = request_hash(kind, &payload);
    let seq = ctx.borrow_mut().next_seq()?;

    let replayed = {
        let ctx = ctx.borrow();
        if kind == "spawn_agent" {
            replay_spawn_agent(&ctx.journal, seq, &payload, &hash)
        } else {
            ctx.journal.replay(seq, kind, &hash)
        }
    };
    match replayed {
        Ok(Some(recorded)) => {
            if let Some(err) = replay_host_error(&recorded) {
                return Err(err);
            }
            return Ok(recorded);
        }
        Ok(None) => {}
        Err(error) => return Err(journal_fatal(error)),
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.borrow()
        .host_tx
        .send(build(reply_tx))
        .map_err(|_| terminated(ControlToken::Fatal("workflow host channel closed".into())))?;

    let reply = reply_rx
        .blocking_recv()
        .map_err(|_| terminated(ControlToken::Fatal("workflow host dropped reply".into())))?;

    let value = match reply {
        Ok(v) => to_result(v),
        Err(HostError::AgentCallQuotaExceeded { requested, maximum }) => {
            return Err(runtime_error(format!(
                "workflow agent-call quota exceeded: requested {requested}, maximum {maximum}"
            )));
        }
        Err(HostError::BudgetExceeded) => {
            return Err(terminated(ControlToken::Budget(
                "workflow agent budget exceeded".into(),
            )));
        }
        Err(HostError::Cancelled) => return Err(terminated(ControlToken::Cancelled)),
        Err(HostError::Unsupported(msg)) => {
            let sentinel = host_error_sentinel(&msg);
            ctx.borrow_mut().record(seq, kind, hash, sentinel)?;
            return Err(runtime_error(msg));
        }
        Err(HostError::Failed(msg)) => {
            let sentinel = host_error_sentinel(&msg);
            ctx.borrow_mut().record(seq, kind, hash, sentinel)?;
            return Err(runtime_error(msg));
        }
    };

    ctx.borrow_mut().record(seq, kind, hash, value.clone())?;
    Ok(value)
}

const HOST_TERMINAL_KEY: &str = "__distill_workflow_parallel_terminal";
const TERMINAL_BUDGET: &str = "budget_exceeded";
const TERMINAL_CANCELLED: &str = "cancelled";
const TERMINAL_DROPPED_REPLY: &str = "dropped_reply";

fn host_error_sentinel(message: &str) -> serde_json::Value {
    serde_json::json!({ HOST_ERROR_KEY: message })
}

fn replay_host_error(recorded: &serde_json::Value) -> Option<Box<rhai::EvalAltResult>> {
    let message = recorded.get(HOST_ERROR_KEY)?.as_str()?;
    Some(runtime_error(message.to_string()))
}

fn host_terminal_sentinel(kind: &str) -> serde_json::Value {
    serde_json::json!({ HOST_TERMINAL_KEY: kind })
}

fn host_terminal_error(kind: &str) -> Box<rhai::EvalAltResult> {
    match kind {
        TERMINAL_BUDGET => terminated(ControlToken::Budget(
            "workflow agent budget exceeded".into(),
        )),
        TERMINAL_CANCELLED => terminated(ControlToken::Cancelled),
        TERMINAL_DROPPED_REPLY => {
            terminated(ControlToken::Fatal("workflow host dropped reply".into()))
        }
        _ => terminated(ControlToken::Fatal(
            "workflow journal contains an unknown terminal marker".into(),
        )),
    }
}

fn is_host_terminal_sentinel(recorded: &serde_json::Value) -> bool {
    recorded.get(HOST_TERMINAL_KEY).is_some()
}

fn host_emit(ctx: &Rc<RefCell<Ctx>>, build: impl FnOnce(bool) -> WorkflowHostRequest) {
    let (replaying, tx) = {
        let ctx = ctx.borrow();
        (ctx.replaying(), ctx.host_tx.clone())
    };
    let _ = tx.send(build(replaying));
}

fn reserve_agent_calls(ctx: &Rc<RefCell<Ctx>>, count: usize) -> ScriptResult<()> {
    if count == 0 {
        return Ok(());
    }
    let count =
        u64::try_from(count).map_err(|_| runtime_error("workflow agent-call count overflowed"))?;
    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.borrow()
        .host_tx
        .send(WorkflowHostRequest::ReserveAgentCalls {
            count,
            reply: reply_tx,
        })
        .map_err(|_| terminated(ControlToken::Fatal("workflow host channel closed".into())))?;
    match reply_rx
        .blocking_recv()
        .map_err(|_| terminated(ControlToken::Fatal("workflow host dropped reply".into())))?
    {
        Ok(()) => Ok(()),
        Err(HostError::AgentCallQuotaExceeded { requested, maximum }) => {
            Err(terminated(ControlToken::Budget(format!(
                "workflow agent budget exceeded: requested {requested}, maximum {maximum}"
            ))))
        }
        Err(HostError::Cancelled) => Err(terminated(ControlToken::Cancelled)),
        Err(HostError::BudgetExceeded) => Err(terminated(ControlToken::Budget(
            "workflow agent budget exceeded".into(),
        ))),
        Err(HostError::Unsupported(message) | HostError::Failed(message)) => {
            Err(runtime_error(message))
        }
    }
}

fn release_agent_calls(ctx: &Rc<RefCell<Ctx>>, count: usize) {
    if count == 0 {
        return;
    }
    let Ok(count) = u64::try_from(count) else {
        return;
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if ctx
        .borrow()
        .host_tx
        .send(WorkflowHostRequest::ReleaseAgentCalls {
            count,
            reply: reply_tx,
        })
        .is_err()
    {
        return;
    }
    let _ = reply_rx.blocking_recv();
}

fn is_resumable_unjournaled_terminal(err: &EvalAltResult) -> bool {
    matches!(
        find_control_token(err),
        Some(ControlToken::Cancelled | ControlToken::Budget(_))
    )
}

fn spawn_agent_call(ctx: &Rc<RefCell<Ctx>>, opts: AgentOpts) -> ScriptResult<Dynamic> {
    let payload = serde_json::to_value(&opts)
        .map_err(|e| runtime_error(format!("invalid agent options: {e}")))?;
    let hash = request_hash("spawn_agent", &payload);
    let is_live = {
        let mut ctx = ctx.borrow_mut();
        ctx.skip_ticket_entries();
        match replay_spawn_agent(&ctx.journal, ctx.seq, &payload, &hash) {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(error) => return Err(journal_fatal(error)),
        }
    };
    if is_live {
        reserve_agent_calls(ctx, 1)?;
    }
    let value = match host_call(
        ctx,
        "spawn_agent",
        payload,
        |reply| WorkflowHostRequest::SpawnAgent {
            opts,
            ticket: None,
            reply,
        },
        |result| serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
    ) {
        Ok(value) => value,
        Err(err) => {
            if is_live && is_resumable_unjournaled_terminal(&err) {
                release_agent_calls(ctx, 1);
            }
            return Err(err);
        }
    };
    value_to_dynamic(&value)
}

fn agent_opts_from_map(prompt: Option<&str>, map: rhai::Map) -> ScriptResult<AgentOpts> {
    let value = map_to_value(map)?;
    let mut opts: AgentOpts = serde_json::from_value(value)
        .map_err(|e| runtime_error(format!("invalid agent options: {e}")))?;
    if let Some(prompt) = prompt {
        opts.prompt = prompt.to_string();
    }
    if opts.prompt.trim().is_empty() {
        return Err(runtime_error("agent prompt must not be empty"));
    }
    Ok(opts)
}

// Tickets: `spawn_agent()` and `spawn_sh()` start a host call and return at once; the script collects the result
// later with `wait_any()` / `wait_all()`, so independent pipelines advance without waiting for each other.
//
// Replay stays deterministic because of what is journaled and when:
// - a ticket's id is its spawn position (`t1`, `t2`, ...), so a resumed script hands out the same ids;
// - a completion is journaled as its own entry the moment the script thread sees it, in arrival order, whether or not
//   the script ever waits on it. On resume the spawn finds that entry by ticket id and does not call the host;
// - every `wait_any()` journals which ticket it returned, or that it timed out, and how long it was blocked, so the
//   script takes the same branches again even though tickets that were still running then are finished by now.
// A reply that ends the run (cancelled, over budget) is not journaled and its agent-call reservation is released,
// like a `parallel()` panel: the resumed run starts that one again and pays for it once.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicketKind {
    Agent,
    Sh,
}

impl TicketKind {
    fn journal_kind(self) -> &'static str {
        match self {
            Self::Agent => TICKET_AGENT_KIND,
            Self::Sh => TICKET_SH_KIND,
        }
    }
}

enum TicketReply {
    Agent(oneshot::Receiver<Result<AgentResult, HostError>>),
    Sh(oneshot::Receiver<Result<ShResult, HostError>>),
}

impl TicketReply {
    /// `Ready(None)` when the host dropped the reply.
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<serde_json::Value, HostError>>> {
        fn settle<T: serde::Serialize>(
            reply: Result<Result<T, HostError>, oneshot::error::RecvError>,
        ) -> Option<Result<serde_json::Value, HostError>> {
            reply.ok().map(|result| {
                result.map(|value| serde_json::to_value(value).unwrap_or(serde_json::Value::Null))
            })
        }
        match self {
            Self::Agent(rx) => Pin::new(rx).poll(cx).map(settle),
            Self::Sh(rx) => Pin::new(rx).poll(cx).map(settle),
        }
    }
}

enum TicketState {
    Live {
        reply: TicketReply,
        cancel: CancellationToken,
    },
    /// Finished and journaled. `value` is the journaled `{ ticket, result }`, plus `error` when the host call
    /// itself failed.
    Done { value: serde_json::Value },
    /// Ended with the run (cancelled, over budget, reply dropped) or could not be journaled. Nothing was recorded,
    /// so a resumed run starts it again.
    Lost,
}

struct Ticket {
    id: String,
    kind: TicketKind,
    hash: String,
    state: TicketState,
    /// Already handed to the script by `wait_any()` or `wait_all()`; `wait_any()` returns a ticket once.
    claimed: bool,
}

fn spawn_ticket(
    ctx: &Rc<RefCell<Ctx>>,
    kind: TicketKind,
    hash: String,
    request: impl FnOnce(TicketHandle) -> (WorkflowHostRequest, TicketReply),
) -> ScriptResult<Dynamic> {
    let (id, recorded) = {
        let ctx = ctx.borrow();
        if ctx.tickets.len() as u64 >= MAX_HOST_CALLS {
            return Err(terminated(ControlToken::Fatal(format!(
                "workflow exceeded the maximum of {MAX_HOST_CALLS} tickets"
            ))));
        }
        let id = format!("t{}", ctx.tickets.len() + 1);
        let entry = ctx
            .recorded_tickets
            .get(&id)
            .and_then(|seq| ctx.journal.entry(*seq));
        let recorded = match entry {
            Some(entry) if entry.kind == kind.journal_kind() && entry.req_hash == hash => {
                Some(entry.result.clone())
            }
            Some(entry) => {
                return Err(journal_fatal(JournalError::Divergence {
                    seq: entry.seq,
                    kind: kind.journal_kind().to_string(),
                }));
            }
            None => None,
        };
        (id, recorded)
    };
    let state = match recorded {
        Some(value) => TicketState::Done { value },
        None => {
            if kind == TicketKind::Agent {
                reserve_agent_calls(ctx, 1)?;
            }
            let cancel = CancellationToken::new();
            let (request, reply) = request(TicketHandle {
                id: id.clone(),
                cancel: cancel.clone(),
            });
            if ctx.borrow().host_tx.send(request).is_err() {
                return Err(terminated(ControlToken::Fatal(
                    "workflow host channel closed".into(),
                )));
            }
            TicketState::Live { reply, cancel }
        }
    };
    ctx.borrow_mut().tickets.push(Ticket {
        id: id.clone(),
        kind,
        hash,
        state,
        claimed: false,
    });
    Ok(id.into())
}

fn spawn_agent_ticket(ctx: &Rc<RefCell<Ctx>>, opts: AgentOpts) -> ScriptResult<Dynamic> {
    let payload = serde_json::to_value(&opts)
        .map_err(|e| runtime_error(format!("invalid agent options: {e}")))?;
    let hash = request_hash("spawn_agent", &payload);
    spawn_ticket(ctx, TicketKind::Agent, hash, |ticket| {
        let (reply, rx) = oneshot::channel();
        (
            WorkflowHostRequest::SpawnAgent {
                opts,
                ticket: Some(ticket),
                reply,
            },
            TicketReply::Agent(rx),
        )
    })
}

fn sh_opts_from_map(map: rhai::Map) -> ScriptResult<ShOpts> {
    let opts: ShOpts = serde_json::from_value(map_to_value(map)?)
        .map_err(|e| runtime_error(format!("invalid sh options: {e}")))?;
    if opts.timeout_ms == Some(0) {
        return Err(runtime_error("sh timeout_ms must be greater than zero"));
    }
    Ok(opts)
}

fn sh_payload(command: &str, opts: &ShOpts) -> ScriptResult<serde_json::Value> {
    if command.trim().is_empty() {
        return Err(runtime_error("sh command must not be empty"));
    }
    Ok(serde_json::json!({ "command": command, "opts": opts }))
}

fn sh_call(ctx: &Rc<RefCell<Ctx>>, command: &str, opts: ShOpts) -> ScriptResult<Dynamic> {
    let payload = sh_payload(command, &opts)?;
    let command = command.to_string();
    let value = host_call(
        ctx,
        "sh",
        payload,
        |reply| WorkflowHostRequest::RunCommand {
            command,
            opts,
            ticket: None,
            reply,
        },
        |result| serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
    )?;
    value_to_dynamic(&value)
}

fn spawn_sh_ticket(ctx: &Rc<RefCell<Ctx>>, command: &str, opts: ShOpts) -> ScriptResult<Dynamic> {
    let payload = sh_payload(command, &opts)?;
    let hash = request_hash("sh", &payload);
    let command = command.to_string();
    spawn_ticket(ctx, TicketKind::Sh, hash, |ticket| {
        let (reply, rx) = oneshot::channel();
        (
            WorkflowHostRequest::RunCommand {
                command,
                opts,
                ticket: Some(ticket),
                reply,
            },
            TicketReply::Sh(rx),
        )
    })
}

/// Polls every running ticket once and journals the ones that replied. A reply that ends the run is returned as the
/// error only after the rest of the pass is journaled, so a ticket that finished in the same instant is kept.
fn sweep_tickets(ctx: &Rc<RefCell<Ctx>>, cx: &mut Context<'_>) -> ScriptResult<()> {
    let mut released = 0usize;
    let mut terminal: Option<Box<EvalAltResult>> = None;
    {
        let mut guard = ctx.borrow_mut();
        let ctx = &mut *guard;
        for index in 0..ctx.tickets.len() {
            let Some(ticket) = ctx.tickets.get_mut(index) else {
                break;
            };
            let TicketState::Live { reply, .. } = &mut ticket.state else {
                continue;
            };
            let Poll::Ready(reply) = reply.poll(cx) else {
                continue;
            };
            let value = match reply {
                Some(Ok(result)) => serde_json::json!({ "ticket": ticket.id, "result": result }),
                Some(Err(error @ (HostError::BudgetExceeded | HostError::Cancelled))) => {
                    ticket.state = TicketState::Lost;
                    if ticket.kind == TicketKind::Agent {
                        released += 1;
                    }
                    terminal.get_or_insert_with(|| {
                        host_terminal_error(if matches!(error, HostError::Cancelled) {
                            TERMINAL_CANCELLED
                        } else {
                            TERMINAL_BUDGET
                        })
                    });
                    continue;
                }
                None => {
                    ticket.state = TicketState::Lost;
                    terminal.get_or_insert_with(|| host_terminal_error(TERMINAL_DROPPED_REPLY));
                    continue;
                }
                Some(Err(HostError::Unsupported(message) | HostError::Failed(message))) => {
                    serde_json::json!({ "ticket": ticket.id, "result": null, "error": message })
                }
                Some(Err(error @ HostError::AgentCallQuotaExceeded { .. })) => {
                    serde_json::json!({ "ticket": ticket.id, "result": null, "error": error.to_string() })
                }
            };
            if let Err(error) = ctx.record_ticket(index, value) {
                terminal = Some(error);
            }
        }
    }
    release_agent_calls(ctx, released);
    match terminal {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Journals the tickets that have already replied, without blocking.
fn settle_tickets(ctx: &Rc<RefCell<Ctx>>) -> ScriptResult<()> {
    if !ctx.borrow().has_live_ticket() {
        return Ok(());
    }
    sweep_tickets(ctx, &mut Context::from_waker(Waker::noop()))
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

enum WaitEnd {
    Ready,
    TimedOut,
}

/// Parks the script thread until `ready` holds or the host fires `timer`, journaling every ticket that replies in the
/// meantime, including ones this wait did not ask for. The engine has no clock of its own: the host owns the timeout.
fn block_on_tickets(
    ctx: &Rc<RefCell<Ctx>>,
    mut timer: Option<oneshot::Receiver<()>>,
    ready: impl Fn(&Ctx) -> bool,
) -> ScriptResult<WaitEnd> {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        sweep_tickets(ctx, &mut cx)?;
        if ready(&ctx.borrow()) {
            return Ok(WaitEnd::Ready);
        }
        if let Some(fired) = timer.as_mut() {
            match Pin::new(fired).poll(&mut cx) {
                Poll::Ready(Ok(())) => return Ok(WaitEnd::TimedOut),
                // The host dropped the timer: this wait has no timeout.
                Poll::Ready(Err(_)) => timer = None,
                Poll::Pending => {}
            }
        }
        if !ctx.borrow().has_live_ticket() {
            return Err(terminated(ControlToken::Fatal(
                "workflow wait has no running ticket left to wait for".into(),
            )));
        }
        std::thread::park();
    }
}

fn ticket_index(ctx: &Ctx, what: &str, id: &str) -> ScriptResult<usize> {
    id.strip_prefix('t')
        .and_then(|number| number.parse::<usize>().ok())
        .and_then(|number| number.checked_sub(1))
        .filter(|index| {
            ctx.tickets
                .get(*index)
                .is_some_and(|ticket| ticket.id == id)
        })
        .ok_or_else(|| runtime_error(format!("{what}(): unknown ticket {id:?}")))
}

fn ticket_indices(
    ctx: &Rc<RefCell<Ctx>>,
    what: &str,
    items: rhai::Array,
) -> ScriptResult<Vec<usize>> {
    let ctx = ctx.borrow();
    let mut indices = Vec::with_capacity(items.len());
    for item in items {
        let id = item.into_immutable_string().map_err(|_| {
            runtime_error(format!(
                "{what}() takes an array of the tickets returned by spawn_agent() and spawn_sh()"
            ))
        })?;
        indices.push(ticket_index(&ctx, what, &id)?);
    }
    Ok(indices)
}

fn ticket_ids(ctx: &Ctx, indices: &[usize], keep: impl Fn(&Ticket) -> bool) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for ticket in indices.iter().filter_map(|index| ctx.tickets.get(*index)) {
        if keep(ticket) && !ids.contains(&ticket.id) {
            ids.push(ticket.id.clone());
        }
    }
    ids
}

/// The finished ticket among `candidates` that was spawned first. Which of several finished tickets a wait returns
/// must not depend on the instant each reply happened to be seen, so the choice is by ticket, not by arrival.
fn lowest_done(ctx: &Ctx, candidates: &[usize]) -> Option<usize> {
    candidates
        .iter()
        .copied()
        .filter(|index| {
            matches!(
                ctx.tickets.get(*index).map(|t| &t.state),
                Some(TicketState::Done { .. })
            )
        })
        .min()
}

/// Marks the finished `tickets[index]` as handed to the script and returns its `{ ticket, result }`.
fn claim_ticket(ctx: &Rc<RefCell<Ctx>>, index: usize) -> ScriptResult<serde_json::Value> {
    let mut ctx = ctx.borrow_mut();
    match ctx.tickets.get_mut(index) {
        Some(ticket) => match &ticket.state {
            TicketState::Done { value } => {
                ticket.claimed = true;
                Ok(value.clone())
            }
            _ => Err(terminated(ControlToken::Fatal(format!(
                "workflow ticket {} has no result",
                ticket.id
            )))),
        },
        None => Err(terminated(ControlToken::Fatal(
            "workflow ticket is out of range".into(),
        ))),
    }
}

fn wait_any(
    ctx: &Rc<RefCell<Ctx>>,
    items: rhai::Array,
    timeout_ms: Option<i64>,
) -> ScriptResult<Dynamic> {
    let timeout_ms = timeout_ms
        .map(|ms| {
            u64::try_from(ms)
                .map_err(|_| runtime_error("wait_any() timeout_ms must not be negative"))
        })
        .transpose()?;
    let given = ticket_indices(ctx, "wait_any", items)?;
    let mut candidates: Vec<usize> = Vec::new();
    let hash = {
        let ctx = ctx.borrow();
        for index in &given {
            let unclaimed = ctx.tickets.get(*index).is_some_and(|t| !t.claimed);
            if unclaimed && !candidates.contains(index) {
                candidates.push(*index);
            }
        }
        let ids: Vec<&str> = given
            .iter()
            .filter_map(|index| ctx.tickets.get(*index).map(|t| t.id.as_str()))
            .collect();
        request_hash(
            "wait_any",
            &serde_json::json!({ "tickets": ids, "timeout_ms": timeout_ms }),
        )
    };
    // Nothing left to hand out. This depends only on what the script already did, so it needs no journal entry.
    if candidates.is_empty() {
        return Ok(Dynamic::UNIT);
    }

    if ctx.borrow().replaying() {
        let seq = ctx.borrow_mut().next_seq()?;
        let recorded = ctx
            .borrow()
            .journal
            .replay(seq, "wait_any", &hash)
            .map_err(journal_fatal)?
            .unwrap_or(serde_json::Value::Null);
        let elapsed_ms = recorded
            .get("elapsed_ms")
            .cloned()
            .unwrap_or_else(|| 0.into());
        if recorded.get("timeout").and_then(|t| t.as_bool()) == Some(true) {
            return value_to_dynamic(
                &serde_json::json!({ "timeout": true, "elapsed_ms": elapsed_ms }),
            );
        }
        let winner = recorded
            .get("ticket")
            .and_then(|id| id.as_str())
            .and_then(|id| {
                let ctx = ctx.borrow();
                candidates.iter().copied().find(|index| {
                    ctx.tickets.get(*index).is_some_and(|ticket| {
                        ticket.id == id && matches!(ticket.state, TicketState::Done { .. })
                    })
                })
            });
        return match winner {
            Some(winner) => waited_ticket(ctx, winner, elapsed_ms),
            None => Err(journal_fatal(JournalError::Divergence {
                seq,
                kind: "wait_any".to_string(),
            })),
        };
    }

    settle_tickets(ctx)?;
    let mut winner = lowest_done(&ctx.borrow(), &candidates);
    // Time is only counted while the script is parked: a ticket that had already finished costs the wait nothing.
    let mut elapsed_ms = 0u64;
    if winner.is_none() && timeout_ms != Some(0) {
        let waiting = ticket_ids(&ctx.borrow(), &candidates, |ticket| {
            matches!(ticket.state, TicketState::Live { .. })
        });
        let (timeout, timer) = match timeout_ms {
            Some(ms) => {
                let (fire, fired) = oneshot::channel();
                (Some(WaitTimeout { ms, fire }), Some(fired))
            }
            None => (None, None),
        };
        // A send that fails drops the timer with it; the tickets then report the missing host themselves.
        let _ = ctx
            .borrow()
            .host_tx
            .send(WorkflowHostRequest::AwaitTickets {
                tickets: waiting,
                all: false,
                timeout,
            });
        let clock = ctx.borrow().clock.clone();
        let blocked_at = clock();
        let end = block_on_tickets(ctx, timer, |ctx| lowest_done(ctx, &candidates).is_some())?;
        elapsed_ms = clock().saturating_sub(blocked_at);
        if matches!(end, WaitEnd::Ready) {
            winner = lowest_done(&ctx.borrow(), &candidates);
        }
    }

    let seq = ctx.borrow_mut().next_seq()?;
    match winner {
        Some(winner) => {
            let id = ctx
                .borrow()
                .tickets
                .get(winner)
                .map(|ticket| ticket.id.clone());
            ctx.borrow_mut().record(
                seq,
                "wait_any",
                hash,
                serde_json::json!({ "ticket": id, "elapsed_ms": elapsed_ms }),
            )?;
            waited_ticket(ctx, winner, elapsed_ms.into())
        }
        None => {
            let timed_out = serde_json::json!({ "timeout": true, "elapsed_ms": elapsed_ms });
            ctx.borrow_mut()
                .record(seq, "wait_any", hash, timed_out.clone())?;
            value_to_dynamic(&timed_out)
        }
    }
}

/// What `wait_any()` returns for a ticket: `#{ ticket, result, elapsed_ms }`, plus `error` when its host call failed.
fn waited_ticket(
    ctx: &Rc<RefCell<Ctx>>,
    index: usize,
    elapsed_ms: serde_json::Value,
) -> ScriptResult<Dynamic> {
    let mut waited = claim_ticket(ctx, index)?;
    if let Some(fields) = waited.as_object_mut() {
        fields.insert("elapsed_ms".to_string(), elapsed_ms);
    }
    value_to_dynamic(&waited)
}

/// Results in the order the tickets were given. Blocking until all of them reply involves no choice the journal
/// would have to remember, so `wait_all()` records nothing beyond the completions themselves.
fn wait_all(ctx: &Rc<RefCell<Ctx>>, items: rhai::Array) -> ScriptResult<rhai::Array> {
    let given = ticket_indices(ctx, "wait_all", items)?;
    // `wait_all([])` is how a script asks whether this engine has tickets at all, so it must touch nothing.
    if given.is_empty() {
        return Ok(rhai::Array::new());
    }
    settle_tickets(ctx)?;
    let waiting = ticket_ids(&ctx.borrow(), &given, |ticket| {
        matches!(ticket.state, TicketState::Live { .. })
    });
    if !waiting.is_empty() {
        let _ = ctx
            .borrow()
            .host_tx
            .send(WorkflowHostRequest::AwaitTickets {
                tickets: waiting,
                all: true,
                timeout: None,
            });
        block_on_tickets(ctx, None, |ctx| {
            given.iter().all(|index| {
                !matches!(
                    ctx.tickets.get(*index).map(|t| &t.state),
                    Some(TicketState::Live { .. })
                )
            })
        })?;
    }
    let mut results = rhai::Array::with_capacity(given.len());
    for index in given {
        let claimed = claim_ticket(ctx, index)?;
        results.push(value_to_dynamic(
            claimed.get("result").unwrap_or(&serde_json::Value::Null),
        )?);
    }
    Ok(results)
}

fn cancel_ticket(ctx: &Rc<RefCell<Ctx>>, id: &str) -> ScriptResult<()> {
    let ctx = ctx.borrow();
    let index = ticket_index(&ctx, "cancel", id)?;
    if let Some(TicketState::Live { cancel, .. }) = ctx.tickets.get(index).map(|t| &t.state) {
        cancel.cancel();
    }
    Ok(())
}

/// Runs once the script has ended, whatever the outcome: waits for the tickets still running and journals their
/// results, like `drain_parallel_replies` does for a panel. A ticket the script never waited on has still been
/// paid for and may have changed the workspace; journaling it means a resumed run (after `await_user`, a raised
/// budget, a pause) gets its result back instead of running it again. A script that wants to stop a ticket rather
/// than wait for it calls `cancel(ticket)` before it ends.
fn drain_tickets(ctx: &Rc<RefCell<Ctx>>, outcome: &WorkflowOutcome) {
    let waiting: Vec<String> = ctx
        .borrow()
        .tickets
        .iter()
        .filter(|ticket| matches!(ticket.state, TicketState::Live { .. }))
        .map(|ticket| ticket.id.clone())
        .collect();
    if waiting.is_empty() {
        return;
    }
    // A run that died on a host error is resumed by pruning that error from the journal's end so the call is
    // retried. The completions appended below would bury it, so prune first, on the condition resume uses.
    if let WorkflowOutcome::Failed { error } = outcome
        && let Err(prune_error) = ctx.borrow_mut().journal.prune_trailing_host_error(error)
    {
        tracing::warn!(%prune_error, "workflow journal: could not prune the failing host call before draining tickets");
    }
    let _ = ctx
        .borrow()
        .host_tx
        .send(WorkflowHostRequest::AwaitTickets {
            tickets: waiting,
            all: true,
            timeout: None,
        });
    while ctx.borrow().has_live_ticket() {
        // An error here is a ticket that ended with the run or could not be journaled; each one leaves the
        // running set, so the loop ends.
        if let Err(error) = block_on_tickets(ctx, None, |ctx| !ctx.has_live_ticket()) {
            tracing::debug!(%error, "workflow ticket dropped while draining");
        }
    }
}

fn register_host_fns(engine: &mut rhai::Engine, ctx: &Rc<RefCell<Ctx>>) {
    let c = ctx.clone();
    engine.register_fn("agent", move |prompt: &str| -> ScriptResult<Dynamic> {
        spawn_agent_call(
            &c,
            AgentOpts {
                prompt: prompt.to_string(),
                ..Default::default()
            },
        )
    });
    let c = ctx.clone();
    engine.register_fn(
        "agent",
        move |prompt: &str, opts: rhai::Map| -> ScriptResult<Dynamic> {
            let opts = agent_opts_from_map(Some(prompt), opts)?;
            spawn_agent_call(&c, opts)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "parallel",
        move |items: rhai::Array| -> ScriptResult<rhai::Array> {
            if items.len() > MAX_PARALLEL {
                return Err(runtime_error(format!(
                    "parallel() accepts at most {MAX_PARALLEL} items per call (got {})",
                    items.len()
                )));
            }
            let mut opts_list = Vec::with_capacity(items.len());
            for item in items {
                let map = item
                    .try_cast::<rhai::Map>()
                    .ok_or_else(|| runtime_error("parallel() items must be option maps"))?;
                opts_list.push(agent_opts_from_map(None, map)?);
            }

            let requests = opts_list
                .into_iter()
                .map(|opts| {
                    let payload = serde_json::to_value(&opts)
                        .map_err(|e| runtime_error(format!("invalid agent options: {e}")))?;
                    let hash = request_hash("spawn_agent", &payload);
                    Ok((opts, payload, hash))
                })
                .collect::<ScriptResult<Vec<_>>>()?;
            let live_count = {
                let mut ctx = c.borrow_mut();
                ctx.skip_ticket_entries();
                let mut seq = ctx.seq;
                let mut live = 0usize;
                for (_, payload, hash) in &requests {
                    match replay_spawn_agent(&ctx.journal, seq, payload, hash) {
                        Ok(Some(_)) => {}
                        Ok(None) => live += 1,
                        Err(error) => return Err(journal_fatal(error)),
                    }
                    seq = seq.checked_add(1).ok_or_else(|| {
                        terminated(ControlToken::Fatal(
                            "workflow host-call count overflowed".into(),
                        ))
                    })?;
                }
                live
            };
            reserve_agent_calls(&c, live_count)?;
            let mut pending = Vec::with_capacity(requests.len());
            for (opts, payload, hash) in requests {
                let seq = c.borrow_mut().next_seq().inspect_err(|_| {
                    drain_parallel_replies(std::mem::take(&mut pending));
                })?;
                match replay_spawn_agent(&c.borrow().journal, seq, &payload, &hash) {
                    Ok(Some(value)) => pending.push(PendingAgent::Replayed(value)),
                    Ok(None) => {
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if c.borrow()
                            .host_tx
                            .send(WorkflowHostRequest::SpawnAgent {
                                opts,
                                ticket: None,
                                reply: reply_tx,
                            })
                            .is_err()
                        {
                            drain_parallel_replies(pending);
                            return Err(terminated(ControlToken::Fatal(
                                "workflow host channel closed".into(),
                            )));
                        }
                        pending.push(PendingAgent::Live {
                            seq,
                            hash,
                            reply_rx,
                        });
                    }
                    Err(error) => {
                        drain_parallel_replies(pending);
                        return Err(journal_fatal(error));
                    }
                }
            }

            let mut resolved: Vec<(Option<(u64, String)>, serde_json::Value)> =
                Vec::with_capacity(pending.len());
            let mut terminal_kind: Option<String> = None;
            let mut terminal_error = None;
            let mut resumable_terminal = false;
            for entry in pending {
                match entry {
                    PendingAgent::Replayed(value) => {
                        if is_host_terminal_sentinel(&value) {
                            let kind = value
                                .get(HOST_TERMINAL_KEY)
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("unknown")
                                .to_string();
                            if kind == TERMINAL_BUDGET || kind == TERMINAL_CANCELLED {
                                terminal_kind.get_or_insert(kind);
                                resumable_terminal = true;
                            } else {
                                terminal_kind.get_or_insert(kind);
                            }
                            continue;
                        }
                        if let Some(error) = replay_host_error(&value) {
                            terminal_error.get_or_insert(error);
                            continue;
                        }
                        resolved.push((None, value));
                    }
                    PendingAgent::Live {
                        seq,
                        hash,
                        reply_rx,
                    } => {
                        let value = match reply_rx.blocking_recv() {
                            Ok(Ok(result)) => {
                                serde_json::to_value(result).unwrap_or(serde_json::Value::Null)
                            }
                            Ok(Err(HostError::BudgetExceeded)) => {
                                terminal_kind.get_or_insert_with(|| TERMINAL_BUDGET.to_string());
                                resumable_terminal = true;
                                host_terminal_sentinel(TERMINAL_BUDGET)
                            }
                            Ok(Err(HostError::Cancelled)) => {
                                terminal_kind.get_or_insert_with(|| TERMINAL_CANCELLED.to_string());
                                resumable_terminal = true;
                                host_terminal_sentinel(TERMINAL_CANCELLED)
                            }
                            Err(_) => {
                                terminal_kind
                                    .get_or_insert_with(|| TERMINAL_DROPPED_REPLY.to_string());
                                host_terminal_sentinel(TERMINAL_DROPPED_REPLY)
                            }
                            Ok(Err(
                                HostError::AgentCallQuotaExceeded { .. }
                                | HostError::Unsupported(_)
                                | HostError::Failed(_),
                            )) => serde_json::Value::Null,
                        };
                        resolved.push((Some((seq, hash)), value));
                    }
                }
            }

            if resumable_terminal {
                release_agent_calls(&c, live_count);
            } else {
                for (live, value) in &resolved {
                    let Some((seq, hash)) = live else {
                        continue;
                    };
                    if let Err(error) =
                        c.borrow_mut()
                            .record(*seq, "spawn_agent", hash.clone(), value.clone())
                    {
                        terminal_error.get_or_insert(error);
                        break;
                    }
                }
            }

            if let Some(kind) = terminal_kind {
                return Err(host_terminal_error(&kind));
            }
            if let Some(error) = terminal_error {
                return Err(error);
            }

            let mut results = rhai::Array::with_capacity(resolved.len());
            for (_, value) in resolved {
                match value_to_dynamic(&value) {
                    Ok(value) => results.push(value),
                    Err(error) => return Err(error),
                }
            }
            Ok(results)
        },
    );

    // Every ticket function is a plain registered function, never syntax (`spawn` itself is a Rhai reserved word):
    // a script that uses them still parses on an engine without them and fails at the call, where it can be caught.
    let c = ctx.clone();
    engine.register_fn(
        "spawn_agent",
        move |opts: rhai::Map| -> ScriptResult<Dynamic> {
            spawn_agent_ticket(&c, agent_opts_from_map(None, opts)?)
        },
    );
    let c = ctx.clone();
    engine.register_fn("sh", move |command: &str| -> ScriptResult<Dynamic> {
        sh_call(&c, command, ShOpts::default())
    });
    let c = ctx.clone();
    engine.register_fn(
        "sh",
        move |command: &str, opts: rhai::Map| -> ScriptResult<Dynamic> {
            sh_call(&c, command, sh_opts_from_map(opts)?)
        },
    );
    let c = ctx.clone();
    engine.register_fn("spawn_sh", move |command: &str| -> ScriptResult<Dynamic> {
        spawn_sh_ticket(&c, command, ShOpts::default())
    });
    let c = ctx.clone();
    engine.register_fn(
        "spawn_sh",
        move |command: &str, opts: rhai::Map| -> ScriptResult<Dynamic> {
            spawn_sh_ticket(&c, command, sh_opts_from_map(opts)?)
        },
    );
    let c = ctx.clone();
    engine.register_fn(
        "wait_any",
        move |tickets: rhai::Array| -> ScriptResult<Dynamic> { wait_any(&c, tickets, None) },
    );
    let c = ctx.clone();
    engine.register_fn(
        "wait_any",
        move |tickets: rhai::Array, timeout_ms: i64| -> ScriptResult<Dynamic> {
            wait_any(&c, tickets, Some(timeout_ms))
        },
    );
    let c = ctx.clone();
    engine.register_fn(
        "wait_all",
        move |tickets: rhai::Array| -> ScriptResult<rhai::Array> { wait_all(&c, tickets) },
    );
    let c = ctx.clone();
    engine.register_fn("cancel", move |ticket: &str| -> ScriptResult<()> {
        cancel_ticket(&c, ticket)
    });

    let c = ctx.clone();
    engine.register_fn("phase", move |title: &str| {
        let title = title.to_string();
        host_emit(&c, |replayed| WorkflowHostRequest::Phase {
            title,
            replayed,
        });
    });

    let c = ctx.clone();
    engine.register_fn("log", move |message: &str| {
        let message = message.to_string();
        host_emit(&c, |replayed| WorkflowHostRequest::Log {
            message,
            replayed,
        });
    });
    let c = ctx.clone();
    engine.on_print(move |message| {
        let message = message.to_string();
        host_emit(&c, |replayed| WorkflowHostRequest::Log {
            message,
            replayed,
        });
    });

    let c = ctx.clone();
    engine.on_debug(move |message, _source, _pos| {
        let message = message.to_string();
        host_emit(&c, |replayed| WorkflowHostRequest::Log {
            message,
            replayed,
        });
    });

    let c = ctx.clone();
    engine.register_fn(
        "telemetry_event",
        move |name: &str, fields: rhai::Map| -> ScriptResult<()> {
            let fields = map_to_value(fields)?;
            let name = name.to_string();
            host_emit(&c, |replayed| WorkflowHostRequest::Telemetry {
                name,
                fields,
                replayed,
            });
            Ok(())
        },
    );

    engine.register_fn("complete", move |value: Dynamic| -> ScriptResult<()> {
        Err(terminated(ControlToken::Complete(dynamic_to_value(value))))
    });
    engine.register_fn("complete", move || -> ScriptResult<()> {
        Err(terminated(ControlToken::Complete(serde_json::Value::Null)))
    });

    engine.register_fn(
        "pause",
        move |kind: &str, message: &str| -> ScriptResult<()> {
            let kind: PauseKind = kind.parse().map_err(|e: String| runtime_error(e))?;
            Err(terminated(ControlToken::Pause(kind, message.to_string())))
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "await_user",
        move |kind: &str, message: &str| -> ScriptResult<()> {
            let parsed: PauseKind = kind.parse().map_err(|e: String| runtime_error(e))?;
            let payload = serde_json::json!({ "kind": kind, "message": message });
            let hash = request_hash("await_user", &payload);
            let seq = c.borrow_mut().next_seq()?;
            let replayed = c.borrow().journal.replay(seq, "await_user", &hash);
            match replayed {
                Ok(Some(_)) => Ok(()),
                Ok(None) => {
                    c.borrow_mut()
                        .record(seq, "await_user", hash, serde_json::Value::Null)?;
                    Err(terminated(ControlToken::Pause(parsed, message.to_string())))
                }
                Err(error) => Err(journal_fatal(error)),
            }
        },
    );

    let c = ctx.clone();
    engine.register_fn("budget", move || -> ScriptResult<Dynamic> {
        let value = host_call(
            &c,
            "budget",
            serde_json::Value::Null,
            |reply| WorkflowHostRequest::BudgetQuery { reply },
            |state| serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
        )?;
        value_to_dynamic(&value)
    });

    let c = ctx.clone();
    engine.register_fn(
        "render_template",
        move |name: &str, vars: rhai::Map| -> ScriptResult<Dynamic> {
            let vars = map_to_value(vars)?;
            let payload = serde_json::json!({ "name": name, "vars": vars });
            let name = name.to_string();
            let value = host_call(
                &c,
                "render_template",
                payload,
                |reply| WorkflowHostRequest::RenderTemplate { name, vars, reply },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "write_scratch_file",
        move |name: &str, content: &str| -> ScriptResult<Dynamic> {
            let payload = serde_json::json!({ "name": name, "content": content });
            let (name, content) = (name.to_string(), content.to_string());
            let value = host_call(
                &c,
                "write_scratch_file",
                payload,
                |reply| WorkflowHostRequest::WriteScratchFile {
                    name,
                    content,
                    reply,
                },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "read_scratch_file",
        move |name: &str| -> ScriptResult<Dynamic> {
            let payload = serde_json::json!({ "name": name });
            let name = name.to_string();
            let value = host_call(
                &c,
                "read_scratch_file",
                payload,
                |reply| WorkflowHostRequest::ReadScratchFile { name, reply },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "git_diff_since",
        move |commit: &str| -> ScriptResult<Dynamic> {
            let payload = serde_json::json!({ "commit": commit });
            let commit = commit.to_string();
            let value = host_call(
                &c,
                "git_diff_since",
                payload,
                |reply| WorkflowHostRequest::GitDiffSince { commit, reply },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    engine.register_fn("fingerprint", |text: &str| -> String {
        crate::journal::request_hash("fingerprint", &serde_json::Value::String(text.to_string()))
    });
    engine.register_fn("json_encode", |value: Dynamic| -> ScriptResult<String> {
        serde_json::to_string(&dynamic_to_value(value))
            .map_err(|error| runtime_error(format!("json encoding failed: {error}")))
    });
    // Rhai's own `parse_json` only accepts an object at the top level; command output is as often an array.
    engine.register_fn("json_decode", |text: &str| -> ScriptResult<Dynamic> {
        let value = serde_json::from_str::<serde_json::Value>(text)
            .map_err(|error| runtime_error(format!("json decoding failed: {error}")))?;
        value_to_dynamic(&value)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{AgentResult, BudgetState};

    fn spawn_mock_host(
        mut rx: mpsc::UnboundedReceiver<WorkflowHostRequest>,
        mut on_request: impl FnMut(WorkflowHostRequest) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while let Some(req) = rx.blocking_recv() {
                match req {
                    WorkflowHostRequest::ReserveAgentCalls { reply, .. }
                    | WorkflowHostRequest::ReleaseAgentCalls { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    other => on_request(other),
                }
            }
        })
    }

    fn spawn_budget_tracking_host(
        mut rx: mpsc::UnboundedReceiver<WorkflowHostRequest>,
        agents_used: std::sync::Arc<std::sync::atomic::AtomicU64>,
        mut on_request: impl FnMut(WorkflowHostRequest) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while let Some(req) = rx.blocking_recv() {
                match req {
                    WorkflowHostRequest::ReserveAgentCalls { count, reply } => {
                        agents_used.fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                        let _ = reply.send(Ok(()));
                    }
                    WorkflowHostRequest::ReleaseAgentCalls { count, reply } => {
                        let _ = agents_used.fetch_update(
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                            |used| Some(used.saturating_sub(count)),
                        );
                        let _ = reply.send(Ok(()));
                    }
                    other => on_request(other),
                }
            }
        })
    }

    fn agent_result(output: &str) -> AgentResult {
        AgentResult {
            agent_id: "child-1".into(),
            success: true,
            output: serde_json::Value::String(output.into()),
            cancelled: false,
            tokens_used: 10,
            duration_ms: 5,
        }
    }

    fn params(
        script: &str,
        journal: Journal,
        host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
    ) -> WorkflowRunParams {
        WorkflowRunParams {
            script: script.to_string(),
            args: serde_json::json!({ "objective": "test" }),
            journal,
            host_tx,
            cancel: CancellationToken::new(),
            max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
            clock: None,
        }
    }

    fn legacy_spawn_agent_hash(opts: &AgentOpts) -> String {
        let mut payload = serde_json::to_value(opts).unwrap();
        assert!(payload.as_object_mut().unwrap().remove("effort").is_some());
        request_hash("spawn_agent", &payload)
    }

    #[test]
    fn happy_path_completes_with_agent_output() {
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| match req {
            WorkflowHostRequest::SpawnAgent { reply, .. } => {
                let _ = reply.send(Ok(agent_result("agent says hi")));
            }
            WorkflowHostRequest::Phase { .. } | WorkflowHostRequest::Log { .. } => {}
            other => panic!("unexpected request: {other:?}"),
        });

        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            phase("Work");
            let r = agent("do it", #{ label: "worker" });
            complete(r.output);
            "#,
            Journal::new(None),
            tx,
        ));
        drop(host);

        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("agent says hi"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn catchable_host_failure_journals_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            let d = "";
            try { d = git_diff_since("abc"); } catch (e) { d = "fallback"; }
            let r = agent("work");
            complete(r.output + ":" + d);
        "#;

        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| match req {
            WorkflowHostRequest::GitDiffSince { reply, .. } => {
                let _ = reply.send(Err(crate::host::HostError::Failed("boom".into())));
            }
            WorkflowHostRequest::SpawnAgent { reply, .. } => {
                let _ = reply.send(Ok(agent_result("one")));
            }
            WorkflowHostRequest::Phase { .. } | WorkflowHostRequest::Log { .. } => {}
            other => panic!("unexpected request: {other:?}"),
        });
        let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("one:fallback"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| match req {
            WorkflowHostRequest::Phase { .. } | WorkflowHostRequest::Log { .. } => {}
            other => panic!("replay must not hit the host: {other:?}"),
        });
        let outcome = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
        drop(host);
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("one:fallback"));
            }
            other => panic!("expected replayed Completed, got {other:?}"),
        }
    }

    #[test]
    fn await_user_pauses_once_then_passes_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            await_user("back_off", "needs a human");
            complete("resumed");
        "#;

        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        match outcome {
            WorkflowOutcome::Paused { kind, message } => {
                assert_eq!(kind, PauseKind::BackOff);
                assert_eq!(message, "needs a human");
            }
            other => panic!("expected Paused, got {other:?}"),
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("resumed"));
            }
            other => panic!("expected Completed after resume, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_is_blocked_for_determinism() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            let t = timestamp();
            complete("unreachable");
            "#,
            Journal::new(None),
            tx,
        ));
        match outcome {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains("deterministic"), "unexpected error: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn args_are_visible_to_script() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            complete(args.objective);
            "#,
            Journal::new(None),
            tx,
        ));
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("test"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn pause_maps_kind() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            pause("back_off", "too many rejections");
            "#,
            Journal::new(None),
            tx,
        ));
        match outcome {
            WorkflowOutcome::Paused { kind, message } => {
                assert_eq!(kind, PauseKind::BackOff);
                assert_eq!(message, "too many rejections");
            }
            other => panic!("expected Paused, got {other:?}"),
        }
    }

    #[test]
    fn cancellation_wins_over_pure_loop() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut p = params(
            r#"
            let meta = #{ name: "t", description: "d" };
            let x = 0;
            loop { x += 1; }
            "#,
            Journal::new(None),
            tx,
        );
        p.cancel = cancel;
        assert!(matches!(run_workflow(p), WorkflowOutcome::Cancelled));
    }

    #[test]
    fn parallel_rejects_oversized_fanout_before_spawning() {
        let (tx, rx) = mpsc::unbounded_channel();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let requests_in_host = requests.clone();
        let host = spawn_mock_host(rx, move |req| {
            requests_in_host.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("unexpected")));
            }
        });
        let script = format!(
            r#"
            let meta = #{{ name: "t", description: "d" }};
            let jobs = [];
            for i in 0..{} {{ jobs.push(#{{ prompt: "job" + i.to_string() }}); }}
            parallel(jobs);
            "#,
            MAX_PARALLEL + 1
        );
        let outcome = run_workflow(params(&script, Journal::new(None), tx));
        drop(host);
        match outcome {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains("parallel() accepts at most"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn host_call_limit_is_non_catchable_and_prevents_sends() {
        let (tx, rx) = mpsc::unbounded_channel();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let requests_in_host = requests.clone();
        let host = spawn_mock_host(rx, move |req| {
            requests_in_host.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let WorkflowHostRequest::BudgetQuery { reply } = req {
                let _ = reply.send(Ok(BudgetState {
                    total: None,
                    spent: 0,
                    reserved: 0,
                    remaining: None,
                }));
            }
        });
        let mut journal = Journal::new(None);
        let hash = request_hash("budget", &serde_json::Value::Null);
        for seq in 0..MAX_HOST_CALLS {
            journal
                .record(
                    seq,
                    "budget",
                    hash.clone(),
                    serde_json::json!({ "total": null, "spent": 0, "reserved": 0, "remaining": null }),
                )
                .unwrap();
        }
        let script = format!(
            r#"
            let meta = #{{ name: "t", description: "d" }};
            for i in 0..{} {{ budget(); }}
            try {{ budget(); }} catch (e) {{ complete("caught"); }}
            complete("unreachable");
            "#,
            MAX_HOST_CALLS
        );
        let outcome = run_workflow(params(&script, journal, tx));
        drop(host);
        match outcome {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains("maximum of"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn parallel_setup_error_drains_already_sent_replies() {
        let (tx, rx) = mpsc::unbounded_channel();
        let first_reply_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first_reply_in_host = first_reply_observed.clone();
        let host = spawn_mock_host(rx, move |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                first_reply_in_host.store(
                    reply.send(Ok(agent_result("drained"))).is_ok(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
        });
        let mut journal = Journal::new(None);
        let hash = request_hash("budget", &serde_json::Value::Null);
        for seq in 0..MAX_HOST_CALLS - 1 {
            journal
                .record(
                    seq,
                    "budget",
                    hash.clone(),
                    serde_json::json!({ "total": null, "spent": 0, "reserved": 0, "remaining": null }),
                )
                .unwrap();
        }
        let script = format!(
            r#"
            let meta = #{{ name: "t", description: "d" }};
            for i in 0..{} {{ budget(); }}
            parallel([#{{ prompt: "live" }}, #{{ prompt: "over-limit" }}]);
            "#,
            MAX_HOST_CALLS - 1
        );
        let outcome = run_workflow(params(&script, journal, tx));
        drop(host);
        match outcome {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains("maximum of"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(first_reply_observed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn parallel_budget_exceeded_leaves_panel_unjournaled_for_raised_cap_resume() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let (tx, rx) = mpsc::unbounded_channel();
        let second_reply_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_reply_in_host = second_reply_observed.clone();
        let mut request_index = 0;
        let host = spawn_mock_host(rx, move |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                if request_index == 0 {
                    let _ = reply.send(Err(HostError::BudgetExceeded));
                } else {
                    second_reply_in_host.store(
                        reply.send(Ok(agent_result("drained"))).is_ok(),
                        std::sync::atomic::Ordering::SeqCst,
                    );
                }
                request_index += 1;
            }
        });
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            parallel([#{ prompt: "first" }, #{ prompt: "second" }]);
        "#;
        let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);
        assert!(matches!(outcome, WorkflowOutcome::BudgetExceeded { .. }));
        assert!(second_reply_observed.load(std::sync::atomic::Ordering::SeqCst));

        let journal = Journal::load(journal_path).unwrap();
        assert_eq!(
            journal.len(),
            0,
            "resumable budget terminal must not journal the parallel panel"
        );

        let (tx, rx) = mpsc::unbounded_channel();
        let live_again = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live_again_host = live_again.clone();
        let host = spawn_mock_host(rx, move |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                live_again_host.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = reply.send(Ok(agent_result("after raise")));
            }
        });
        let replay = run_workflow(params(script, journal, tx));
        drop(host);
        assert!(matches!(replay, WorkflowOutcome::Completed { .. }));
        assert_eq!(live_again.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn cancelled_live_agent_releases_budget_so_resume_does_not_double_charge() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let agents_used = std::sync::Arc::new(AtomicU64::new(0));
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            let r = agent("work");
            complete(r.output);
        "#;

        {
            let used = agents_used.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let host = spawn_budget_tracking_host(rx, used, |req| {
                if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                    let _ = reply.send(Err(HostError::Cancelled));
                }
            });
            let outcome =
                run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
            drop(host);
            assert!(matches!(outcome, WorkflowOutcome::Cancelled));
            assert_eq!(
                agents_used.load(Ordering::SeqCst),
                0,
                "cancelled agent must ReleaseAgentCalls the reserved slot"
            );
            assert_eq!(
                Journal::load(journal_path.clone()).unwrap().len(),
                0,
                "cancelled agent must leave the spawn unjournaled"
            );
        }

        {
            let used = agents_used.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let host = spawn_budget_tracking_host(rx, used, |req| {
                if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                    let _ = reply.send(Ok(agent_result("after resume")));
                }
            });
            let outcome = run_workflow(params(
                script,
                Journal::load(journal_path.clone()).unwrap(),
                tx,
            ));
            drop(host);
            match outcome {
                WorkflowOutcome::Completed { result } => {
                    assert_eq!(result, serde_json::json!("after resume"));
                }
                other => panic!("expected Completed after resume, got {other:?}"),
            }
            assert_eq!(
                agents_used.load(Ordering::SeqCst),
                1,
                "resume must not double-charge a slot left over from cancelled first run"
            );
        }
    }

    #[test]
    fn cancelled_parallel_releases_budget_so_resume_does_not_double_charge() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let agents_used = std::sync::Arc::new(AtomicU64::new(0));
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([#{ prompt: "first" }, #{ prompt: "second" }]);
            complete(results);
        "#;

        {
            let used = agents_used.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let host = spawn_budget_tracking_host(rx, used, |req| {
                if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                    let _ = reply.send(Err(HostError::Cancelled));
                }
            });
            let outcome =
                run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
            drop(host);
            assert!(matches!(outcome, WorkflowOutcome::Cancelled));
            assert_eq!(
                agents_used.load(Ordering::SeqCst),
                0,
                "cancelled parallel must release live_count reserved slots"
            );
            assert_eq!(Journal::load(journal_path.clone()).unwrap().len(), 0);
        }

        {
            let used = agents_used.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let host = spawn_budget_tracking_host(rx, used, |req| {
                if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                    let _ = reply.send(Ok(agent_result("ok")));
                }
            });
            let outcome = run_workflow(params(
                script,
                Journal::load(journal_path.clone()).unwrap(),
                tx,
            ));
            drop(host);
            assert!(matches!(outcome, WorkflowOutcome::Completed { .. }));
            assert_eq!(
                agents_used.load(Ordering::SeqCst),
                2,
                "resume must charge exactly live_count, not leftover + live_count"
            );
        }
    }

    #[test]
    fn budget_exceeded_live_agent_releases_budget_so_resume_does_not_double_charge() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let agents_used = std::sync::Arc::new(AtomicU64::new(0));
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            let r = agent("work");
            complete(r.output);
        "#;

        {
            let used = agents_used.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let host = spawn_budget_tracking_host(rx, used, |req| {
                if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                    let _ = reply.send(Err(HostError::BudgetExceeded));
                }
            });
            let outcome =
                run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
            drop(host);
            assert!(matches!(outcome, WorkflowOutcome::BudgetExceeded { .. }));
            assert_eq!(
                agents_used.load(Ordering::SeqCst),
                0,
                "budget-exceeded agent must ReleaseAgentCalls the reserved slot"
            );
            assert_eq!(Journal::load(journal_path.clone()).unwrap().len(), 0);
        }

        {
            let used = agents_used.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            let host = spawn_budget_tracking_host(rx, used, |req| {
                if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                    let _ = reply.send(Ok(agent_result("after raise")));
                }
            });
            let outcome = run_workflow(params(
                script,
                Journal::load(journal_path.clone()).unwrap(),
                tx,
            ));
            drop(host);
            match outcome {
                WorkflowOutcome::Completed { result } => {
                    assert_eq!(result, serde_json::json!("after raise"));
                }
                other => panic!("expected Completed after resume, got {other:?}"),
            }
            assert_eq!(
                agents_used.load(Ordering::SeqCst),
                1,
                "resume after budget terminal must not double-charge"
            );
        }
    }

    #[test]
    fn parallel_journals_soft_failure_null_and_later_success() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let (tx, rx) = mpsc::unbounded_channel();
        let mut request_index = 0;
        let host = spawn_mock_host(rx, move |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                if request_index == 0 {
                    let _ = reply.send(Err(HostError::Failed("boom".into())));
                } else {
                    let _ = reply.send(Ok(agent_result("ok")));
                }
                request_index += 1;
            }
        });
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([#{ prompt: "first" }, #{ prompt: "second" }]);
            complete(results);
        "#;
        let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);
        assert!(matches!(outcome, WorkflowOutcome::Completed { .. }));
        let journal = Journal::load(journal_path).unwrap();
        assert_eq!(journal.len(), 2);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let replay = run_workflow(params(script, journal, tx));
        assert!(matches!(replay, WorkflowOutcome::Completed { .. }));
        assert!(
            rx.try_recv().is_err(),
            "dense soft-failure replay must not reexecute either sibling"
        );
    }

    #[test]
    fn parallel_replays_catchable_failure_sentinel() {
        let mut journal = Journal::new(None);
        let opts = AgentOpts {
            prompt: "replayed".into(),
            ..Default::default()
        };
        let hash = request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap());
        journal
            .record(
                0,
                "spawn_agent",
                hash,
                host_error_sentinel("replayed failure"),
            )
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            try {
                parallel([#{ prompt: "replayed" }]);
                complete("not caught");
            } catch (e) {
                complete("caught:" + e);
            }
            "#,
            journal,
            tx,
        ));
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("caught:replayed failure"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn parallel_preserves_order_and_nulls_failures() {
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| {
            if let WorkflowHostRequest::SpawnAgent { opts, reply, .. } = req {
                if opts.prompt.contains("fail") {
                    let _ = reply.send(Err(HostError::Failed("boom".into())));
                } else {
                    let _ = reply.send(Ok(agent_result(&format!("ok:{}", opts.prompt))));
                }
            }
        });
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([
                #{ prompt: "a" },
                #{ prompt: "fail-b" },
                #{ prompt: "c" },
            ]);
            let summary = results.map(|r| if r == () { "null" } else { r.output });
            complete(summary);
            "#,
            Journal::new(None),
            tx,
        ));
        drop(host);
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!(["ok:a", "null", "ok:c"]));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn agent_replays_legacy_hash_without_effort() {
        let opts = AgentOpts {
            prompt: "legacy single".into(),
            effort: Some("high".into()),
            ..Default::default()
        };
        let legacy_hash = legacy_spawn_agent_hash(&opts);
        assert_ne!(
            legacy_hash,
            request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap())
        );
        let mut journal = Journal::new(None);
        journal
            .record(
                0,
                "spawn_agent",
                legacy_hash,
                serde_json::to_value(agent_result("legacy result")).unwrap(),
            )
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            let result = agent("legacy single", #{ effort: "high" });
            complete(result.output);
            "#,
            journal,
            tx,
        ));

        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("legacy result"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "replay must not hit the host");
    }

    #[test]
    fn parallel_replays_legacy_hashes_without_effort() {
        let opts = [
            AgentOpts {
                prompt: "legacy first".into(),
                effort: Some("low".into()),
                ..Default::default()
            },
            AgentOpts {
                prompt: "legacy second".into(),
                effort: Some("high".into()),
                ..Default::default()
            },
        ];
        let mut journal = Journal::new(None);
        for (seq, (opts, output)) in opts
            .iter()
            .zip(["first result", "second result"])
            .enumerate()
        {
            journal
                .record(
                    seq as u64,
                    "spawn_agent",
                    legacy_spawn_agent_hash(opts),
                    serde_json::to_value(agent_result(output)).unwrap(),
                )
                .unwrap();
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([
                #{ prompt: "legacy first", effort: "low" },
                #{ prompt: "legacy second", effort: "high" },
            ]);
            complete(results.map(|result| result.output));
            "#,
            journal,
            tx,
        ));

        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!(["first result", "second result"]));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "replay must not hit the host");
    }

    #[test]
    fn new_agent_recording_hash_includes_effort() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("recorded")));
            }
        });
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            agent("new recording", #{ effort: "high" });
            complete("ok");
            "#,
            Journal::new(Some(journal_path.clone())),
            tx,
        ));
        drop(host);
        assert!(matches!(outcome, WorkflowOutcome::Completed { .. }));

        let opts = AgentOpts {
            prompt: "new recording".into(),
            effort: Some("high".into()),
            ..Default::default()
        };
        let journal = Journal::load(journal_path).unwrap();
        let current_hash = request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap());
        assert!(
            journal
                .replay(0, "spawn_agent", &current_hash)
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            journal.replay(0, "spawn_agent", &legacy_spawn_agent_hash(&opts)),
            Err(JournalError::Divergence { .. })
        ));
    }

    #[test]
    fn agent_current_hash_mismatch_still_diverges() {
        let opts = AgentOpts {
            prompt: "original prompt".into(),
            effort: Some("high".into()),
            ..Default::default()
        };
        let mut journal = Journal::new(None);
        journal
            .record(
                0,
                "spawn_agent",
                request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap()),
                serde_json::to_value(agent_result("must not replay")).unwrap(),
            )
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            agent("edited prompt", #{ effort: "high" });
            "#,
            journal,
            tx,
        ));

        match outcome {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains("divergence"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "divergent replay must not hit the host"
        );
    }

    #[test]
    fn journal_replay_skips_host_calls() {
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            let a = agent("first");
            let b = budget();
            complete(#{ out: a.output, spent: b.spent, reserved: b.reserved, remaining: b.remaining });
        "#;

        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");

        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| match req {
            WorkflowHostRequest::SpawnAgent { reply, .. } => {
                let _ = reply.send(Ok(agent_result("recorded output")));
            }
            WorkflowHostRequest::BudgetQuery { reply } => {
                let _ = reply.send(Ok(BudgetState {
                    total: Some(1000),
                    spent: 123,
                    reserved: 100,
                    remaining: Some(777),
                }));
            }
            _ => {}
        });
        let first = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);
        let WorkflowOutcome::Completed { result: first } = first else {
            panic!("first run should complete");
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| match req {
            WorkflowHostRequest::SpawnAgent { .. } | WorkflowHostRequest::BudgetQuery { .. } => {
                panic!("replay must not hit the host")
            }
            _ => {}
        });
        let second = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
        drop(host);
        let WorkflowOutcome::Completed { result: second } = second else {
            panic!("resumed run should complete");
        };
        assert_eq!(first, second);
        assert_eq!(second.get("spent"), Some(&serde_json::json!(123)));
        assert_eq!(second.get("reserved"), Some(&serde_json::json!(100)));
        assert_eq!(second.get("remaining"), Some(&serde_json::json!(777)));
    }

    #[test]
    fn journal_write_failure_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");
        std::fs::create_dir(&journal_path).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("unpersisted")));
            }
        });
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            agent("work");
            complete("must not complete");
            "#,
            Journal::new(Some(journal_path)),
            tx,
        ));
        drop(host);
        match outcome {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains("journal io"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn journal_divergence_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");

        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("v1")));
            }
        });
        let first = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            agent("original prompt");
            complete("ok");
            "#,
            Journal::new(Some(journal_path.clone())),
            tx,
        ));
        drop(host);
        assert!(matches!(first, WorkflowOutcome::Completed { .. }));

        let (tx, _rx) = mpsc::unbounded_channel();
        let second = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            agent("EDITED prompt");
            complete("ok");
            "#,
            Journal::load(journal_path).unwrap(),
            tx,
        ));
        match second {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains("divergence"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn phase_carries_replay_flag() {
        let script = r#"
            let meta = #{ name: "t", description: "d" };
            phase("One");
            agent("x");
            phase("Two");
            complete("ok");
        "#;
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("journal.jsonl");

        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_mock_host(rx, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("y")));
            }
        });
        let _ = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);

        let (tx, rx) = mpsc::unbounded_channel();
        let phases = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let phases_in_host = phases.clone();
        let host = spawn_mock_host(rx, move |req| {
            if let WorkflowHostRequest::Phase { title, replayed } = req {
                phases_in_host.lock().unwrap().push((title, replayed));
            }
        });
        let _ = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
        host.join().unwrap();

        let phases = phases.lock().unwrap();
        assert_eq!(
            phases.as_slice(),
            &[("One".into(), true), ("Two".into(), false)]
        );
    }

    /// Command output and agent text arrive as strings. Rhai's `parse_json` rejects anything but an object, so
    /// `json_decode` has to take every JSON value, and a script must be able to catch text that is not JSON.
    #[test]
    fn json_decode_reads_any_json_value_and_fails_catchably() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            let list = json_decode("[1, {\"room\": \"sala\", \"ok\": true, \"none\": null}, 2.5]");
            let bad = "";
            try { json_decode("not json"); } catch (e) { bad = "caught"; }
            complete(#{
                first: list[0], room: list[1].room, none: list[1].none == (), text: json_decode("\"x\""),
                bad: bad, round_trip: json_decode(json_encode(#{ a: [1, 2] })).a[1],
            });
            "#,
            Journal::new(None),
            tx,
        ));
        match outcome {
            WorkflowOutcome::Completed { result } => assert_eq!(
                result,
                serde_json::json!({
                    "first": 1, "room": "sala", "none": true, "text": "x", "bad": "caught", "round_trip": 2
                })
            ),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn json_encode_quotes_untrusted_strings() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = run_workflow(params(
            r#"
            let meta = #{ name: "t", description: "d" };
            complete(json_encode("</tag>\nquoted"));
            "#,
            Journal::new(None),
            tx,
        ));
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("\"</tag>\\nquoted\""));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}

#[cfg(test)]
#[path = "engine_ticket_tests.rs"]
mod ticket_tests;
