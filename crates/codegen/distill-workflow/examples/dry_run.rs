// Modified for Distill by Samuel Fajreldines, 2026.
//! Dry-runs a workflow script with real args and scripted agent results, and prints the trace.
//!
//! `validate` answers "does the script parse and reach an outcome with the probe args". This answers "which agents
//! does this path launch, in what order, how does it end, and how long would it take", without a model call: every
//! `agent()` returns a value built from its `output_schema`, overridden by the scenario, and nothing really runs.
//!
//! ```text
//! cargo run -p distill-workflow --example dry_run -- <script.rhai> '<args json>' ['<scenario json>']
//! ```
//!
//! The scenario is an array of rules, tried in order. An agent takes the first rule whose `label` is a substring of
//! the agent label; a shell command (`sh`, `spawn_sh`) takes the first rule whose `sh` is a substring of the command:
//!
//! ```json
//! [{ "label": ":triagem", "outputs": [{ "projeto": 2 }, { "projeto": 0 }], "ms": 90000 },
//!  { "label": ":detalhe", "fail": true },
//!  { "sh": "bun render.ts", "stdout": "{\"ok\":true}", "code": 0, "ms": 4000 }]
//! ```
//!
//! The nth call matched by a rule gets its nth output (the last one repeats), merged over the schema defaults.
//! `"fail": true` returns an unsuccessful agent.
//!
//! Time is virtual. A call lasts its rule's `"ms"` (1000 when absent, or when no rule matches). Blocking calls add
//! up, a `parallel()` lasts as long as its slowest item, and a ticket (`spawn_agent`, `spawn_sh`) runs from its spawn
//! until its own finish while the script goes on. `wait_any` gets the awaited ticket that finishes first and its
//! `elapsed_ms` is the virtual time that took; a ticket whose finish is already in the past comes back at once
//! (the lowest such ticket, `elapsed_ms: 0`); and a timeout that comes sooner moves the clock by the timeout. The
//! run ends with the timeline and the total, so two ways of orchestrating the same work can be compared.
//! Concurrency is unlimited here; the real host queues beyond its live-agent cap.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use distill_workflow::{
    AgentResult, BudgetState, Journal, ShResult, TicketHandle, WorkflowHostRequest,
    WorkflowOutcome, WorkflowRunParams, extract_meta, run_workflow,
};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

const DEFAULT_MS: u64 = 1000;

/// A value of the shape the schema asks for: `true`, `0`, `"stub"`, `[]`, `{}`.
fn default_for(schema: &Value) -> Value {
    match schema.get("type").and_then(Value::as_str) {
        Some("boolean") => json!(true),
        Some("number") | Some("integer") => json!(0),
        Some("string") => json!("stub"),
        Some("array") => json!([]),
        Some("object") => {
            let mut out = Map::new();
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (key, prop) in props {
                    out.insert(key.clone(), default_for(prop));
                }
            }
            Value::Object(out)
        }
        _ => Value::Null,
    }
}

struct Rule {
    /// Matches agents by label.
    label: Option<String>,
    /// Matches shell commands by command text.
    sh: Option<String>,
    outputs: Vec<Value>,
    fail: bool,
    ms: u64,
    stdout: String,
    code: i64,
    calls: usize,
}

fn parse_scenario(raw: Option<&str>) -> Vec<Rule> {
    let Some(raw) = raw else { return Vec::new() };
    let rules: Vec<Value> = serde_json::from_str(raw).expect("scenario must be a JSON array");
    rules
        .into_iter()
        .map(|rule| {
            let text = |key: &str| rule.get(key).and_then(Value::as_str).map(str::to_string);
            let (label, sh) = (text("label"), text("sh"));
            assert!(
                label.is_some() || sh.is_some(),
                "each scenario rule needs a `label` (agents) or a `sh` (shell commands)"
            );
            Rule {
                label,
                sh,
                outputs: rule
                    .get("outputs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                fail: rule.get("fail").and_then(Value::as_bool).unwrap_or(false),
                ms: rule.get("ms").and_then(Value::as_u64).unwrap_or(DEFAULT_MS),
                stdout: text("stdout").unwrap_or_default(),
                code: rule.get("code").and_then(Value::as_i64).unwrap_or(0),
                calls: 0,
            }
        })
        .collect()
}

/// One agent or command on the virtual timeline.
struct Span {
    kind: &'static str,
    name: String,
    ticket: Option<String>,
    start: u64,
    finish: u64,
    note: &'static str,
}

enum Reply {
    Agent(
        oneshot::Sender<Result<AgentResult, distill_workflow::HostError>>,
        AgentResult,
    ),
    Sh(
        oneshot::Sender<Result<ShResult, distill_workflow::HostError>>,
        ShResult,
    ),
}

impl Reply {
    fn send(self, cancelled: bool) {
        match self {
            Reply::Agent(reply, mut result) => {
                if cancelled {
                    result.success = false;
                    result.cancelled = true;
                }
                let _ = reply.send(Ok(result));
            }
            Reply::Sh(reply, mut result) => {
                if cancelled {
                    result = ShResult {
                        cancelled: true,
                        duration_ms: result.duration_ms,
                        ..Default::default()
                    };
                }
                let _ = reply.send(Ok(result));
            }
        }
    }
}

/// A ticketed call that is running in virtual time and has not been handed to the script yet.
struct Running {
    ticket: TicketHandle,
    span: usize,
    reply: Reply,
    cancelled: bool,
}

#[derive(Default)]
struct Sim {
    rules: Vec<Rule>,
    /// The virtual clock, shared with the engine: it is what `wait_any` measures `elapsed_ms` on. It is always moved
    /// before the reply that wakes the script is sent.
    clock: Arc<AtomicU64>,
    spans: Vec<Span>,
    running: Vec<Running>,
    /// Items of the `parallel()` (or single `agent()`) being admitted: how many are left, how many there were, when it
    /// started and when its slowest item ends.
    batch_left: u64,
    batch_size: u64,
    batch_start: u64,
    batch_end: u64,
    agents: u64,
    seen: HashMap<String, usize>,
}

impl Sim {
    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn advance_to(&self, instant: u64) {
        self.clock.fetch_max(instant, Ordering::SeqCst);
    }

    /// `cancel(ticket)` carries no host request, but the clock only moves inside requests, so a ticket found
    /// cancelled at the start of one was cancelled at the current instant.
    fn note_cancellations(&mut self) {
        let now = self.now();
        for running in &mut self.running {
            if running.cancelled || !running.ticket.cancel.is_cancelled() {
                continue;
            }
            running.cancelled = true;
            if let Some(span) = self.spans.get_mut(running.span) {
                span.finish = span.finish.min(now).max(span.start);
                span.note = "cancelled";
            }
        }
    }

    fn agent(
        &mut self,
        opts: distill_workflow::AgentOpts,
        ticket: Option<TicketHandle>,
        reply: oneshot::Sender<Result<AgentResult, distill_workflow::HostError>>,
    ) {
        self.agents += 1;
        let agents = self.agents;
        let label = opts.label.clone().unwrap_or_else(|| "(sem label)".into());
        let mut output = opts
            .output_schema
            .as_ref()
            .map(default_for)
            .unwrap_or_else(|| json!({ "ok": true }));
        let mut success = true;
        let mut ms = DEFAULT_MS;
        if let Some(rule) = self
            .rules
            .iter_mut()
            .find(|r| r.label.as_ref().is_some_and(|l| label.contains(l)))
        {
            let nth = rule.calls.min(rule.outputs.len().saturating_sub(1));
            rule.calls += 1;
            if let (Some(Value::Object(patch)), Value::Object(base)) =
                (rule.outputs.get(nth), &mut output)
            {
                for (key, value) in patch {
                    base.insert(key.clone(), value.clone());
                }
            }
            success = !rule.fail;
            ms = rule.ms;
        }
        let repeat = self.seen.entry(label.clone()).or_insert(0);
        *repeat += 1;
        let repeat = *repeat;

        let in_batch = ticket.is_none() && self.batch_left > 0;
        let marker = match &ticket {
            Some(ticket) => format!(" ⇢{}", ticket.id),
            None if in_batch && self.batch_size > 1 => format!(
                " ∥{}/{}",
                self.batch_size - self.batch_left + 1,
                self.batch_size
            ),
            None => String::new(),
        };
        println!(
            "AGENT {agents:>3}{marker} {label}{} type={} model={}{}{}",
            if repeat > 1 {
                format!(" (#{repeat})")
            } else {
                String::new()
            },
            opts.agent_type.as_deref().unwrap_or("-"),
            opts.model.as_deref().unwrap_or("-"),
            if opts.resume_from.is_some() {
                " resume"
            } else {
                ""
            },
            if success { "" } else { " FAIL" },
        );

        let result = AgentResult {
            agent_id: format!("stub-{agents}"),
            success,
            output,
            cancelled: false,
            tokens_used: 1_000,
            duration_ms: ms,
        };
        // A blocking call in a batch starts when the batch did; everything else starts now.
        let start = if in_batch {
            self.batch_start
        } else {
            self.now()
        };
        let finish = start + ms;
        self.spans.push(Span {
            kind: "agent",
            name: label,
            ticket: ticket.as_ref().map(|t| t.id.clone()),
            start,
            finish,
            note: if success { "" } else { "FAIL" },
        });
        match ticket {
            Some(ticket) => {
                // The reservation that preceded this spawn was for the ticket, not for a blocking batch.
                self.batch_left = 0;
                self.running.push(Running {
                    ticket,
                    span: self.spans.len() - 1,
                    reply: Reply::Agent(reply, result),
                    cancelled: false,
                });
            }
            None => {
                if in_batch {
                    self.batch_end = self.batch_end.max(finish);
                    self.batch_left -= 1;
                    if self.batch_left == 0 {
                        self.advance_to(self.batch_end);
                    }
                } else {
                    self.advance_to(finish);
                }
                let _ = reply.send(Ok(result));
            }
        }
    }

    fn command(
        &mut self,
        command: String,
        opts: distill_workflow::ShOpts,
        ticket: Option<TicketHandle>,
        reply: oneshot::Sender<Result<ShResult, distill_workflow::HostError>>,
    ) {
        let (mut ms, mut stdout, mut code) = (DEFAULT_MS, String::new(), 0);
        if let Some(rule) = self
            .rules
            .iter_mut()
            .find(|r| r.sh.as_ref().is_some_and(|s| command.contains(s)))
        {
            rule.calls += 1;
            (ms, stdout, code) = (rule.ms, rule.stdout.clone(), rule.code);
        }
        let limit = opts
            .timeout_ms
            .unwrap_or(distill_workflow::DEFAULT_SH_TIMEOUT_MS);
        let timed_out = ms > limit;
        let ms = ms.min(limit);
        let name = opts.label.clone().unwrap_or_else(|| command.clone());
        println!(
            "SH{} {command}{}",
            ticket
                .as_ref()
                .map(|t| format!(" ⇢{}", t.id))
                .unwrap_or_default(),
            if timed_out {
                " TIMEOUT".to_string()
            } else if code != 0 {
                format!(" exit={code}")
            } else {
                String::new()
            },
        );
        let result = ShResult {
            ok: !timed_out && code == 0,
            code: (!timed_out).then_some(code),
            stdout: if timed_out { String::new() } else { stdout },
            timed_out,
            duration_ms: ms,
            ..Default::default()
        };
        let start = self.now();
        self.spans.push(Span {
            kind: "sh",
            name,
            ticket: ticket.as_ref().map(|t| t.id.clone()),
            start,
            finish: start + ms,
            note: if timed_out {
                "timeout"
            } else if code != 0 {
                "FAIL"
            } else {
                ""
            },
        });
        match ticket {
            Some(ticket) => self.running.push(Running {
                ticket,
                span: self.spans.len() - 1,
                reply: Reply::Sh(reply, result),
                cancelled: false,
            }),
            None => {
                self.advance_to(start + ms);
                let _ = reply.send(Ok(result));
            }
        }
    }

    fn finish_of(&self, running: &Running) -> u64 {
        self.spans
            .get(running.span)
            .map_or(self.now(), |s| s.finish)
    }

    /// Hands the script the running call at `index`, moving the clock to its finish first.
    fn deliver(&mut self, index: usize) {
        let running = self.running.remove(index);
        self.advance_to(self.finish_of(&running));
        running.reply.send(running.cancelled);
    }

    /// The script is blocked: advance to the next thing it is waiting for. `running` is in spawn order, so the
    /// first match is the lowest ticket.
    fn wait(
        &mut self,
        tickets: Vec<String>,
        all: bool,
        timeout: Option<distill_workflow::WaitTimeout>,
    ) {
        let awaited = |running: &Running| tickets.contains(&running.ticket.id);
        if all {
            // Every awaited ticket, in finish order; the clock ends on the last one.
            while let Some(index) = self
                .running
                .iter()
                .enumerate()
                .filter(|(_, running)| awaited(running))
                .min_by_key(|(index, running)| (self.finish_of(running), *index))
                .map(|(index, _)| index)
            {
                self.deliver(index);
            }
            return;
        }
        let now = self.now();
        // Finished while the script was busy elsewhere: the lowest such ticket, and the clock does not move.
        if let Some(index) = self
            .running
            .iter()
            .position(|running| awaited(running) && self.finish_of(running) <= now)
        {
            self.deliver(index);
            return;
        }
        let next = self
            .running
            .iter()
            .enumerate()
            .filter(|(_, running)| awaited(running))
            .min_by_key(|(index, running)| (self.finish_of(running), *index))
            .map(|(index, running)| (index, self.finish_of(running)));
        match (next, timeout) {
            (Some((_, finish)), Some(timeout)) if now + timeout.ms < finish => {
                self.advance_to(now + timeout.ms);
                println!("TIMEOUT wait_any gave up after {} ms", timeout.ms);
                let _ = timeout.fire.send(());
            }
            (Some((index, _)), _) => self.deliver(index),
            (None, _) => {
                println!("WARN the script waits on tickets that are not running: {tickets:?}")
            }
        }
    }

    fn print_timeline(&self) {
        println!("TIMELINE (virtual ms)");
        let mut order: Vec<&Span> = self.spans.iter().collect();
        order.sort_by_key(|span| span.start);
        for span in order {
            println!(
                "  {:>9} → {:>9}  {:<5} {}{}{}",
                span.start,
                span.finish,
                span.kind,
                span.name,
                span.ticket
                    .as_ref()
                    .map(|t| format!(" [{t}]"))
                    .unwrap_or_default(),
                if span.note.is_empty() {
                    String::new()
                } else {
                    format!(" {}", span.note)
                },
            );
        }
        println!("ELAPSED {} ms (virtual)", self.now());
    }
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let path = argv
        .next()
        .expect("usage: dry_run <script.rhai> '<args json>' ['<scenario json>']");
    let args: Value = serde_json::from_str(&argv.next().unwrap_or_else(|| "{}".into()))
        .expect("args must be JSON");
    let scenario = argv.next();
    let script = std::fs::read_to_string(&path).expect("read script");

    match extract_meta(&script) {
        Ok(meta) => println!("META name={} phases={}", meta.name, meta.phases.len()),
        Err(error) => {
            println!("META FAIL: {error}");
            std::process::exit(1);
        }
    }

    let (host_tx, mut host_rx) = tokio::sync::mpsc::unbounded_channel();
    let clock = Arc::new(AtomicU64::new(0));
    let script_clock = clock.clone();
    let host = std::thread::spawn(move || {
        use WorkflowHostRequest as R;
        let mut sim = Sim {
            rules: parse_scenario(scenario.as_deref()),
            clock,
            ..Default::default()
        };
        while let Some(req) = host_rx.blocking_recv() {
            sim.note_cancellations();
            match req {
                R::ReserveAgentCalls { count, reply } => {
                    sim.batch_left = count;
                    sim.batch_size = count;
                    sim.batch_start = sim.now();
                    sim.batch_end = sim.now();
                    let _ = reply.send(Ok(()));
                }
                R::ReleaseAgentCalls { reply, .. } => {
                    let _ = reply.send(Ok(()));
                }
                R::SpawnAgent {
                    opts,
                    ticket,
                    reply,
                } => sim.agent(opts, ticket, reply),
                R::RunCommand {
                    command,
                    opts,
                    ticket,
                    reply,
                } => sim.command(command, opts, ticket, reply),
                R::AwaitTickets {
                    tickets,
                    all,
                    timeout,
                } => sim.wait(tickets, all, timeout),
                R::BudgetQuery { reply } => {
                    let _ = reply.send(Ok(BudgetState {
                        total: None,
                        spent: 0,
                        reserved: 0,
                        remaining: None,
                    }));
                }
                R::RenderTemplate { reply, .. } => {
                    let _ = reply.send(Ok("stub template".into()));
                }
                R::WriteScratchFile { name, reply, .. } => {
                    let _ = reply.send(Ok(format!("scratch/{name}")));
                }
                R::ReadScratchFile { reply, .. } => {
                    let _ = reply.send(Ok("stub content".into()));
                }
                R::GitDiffSince { reply, .. } => {
                    let _ = reply.send(Ok(String::new()));
                }
                R::Phase { title, .. } => println!("PHASE {title}"),
                R::Log { message, .. } => println!("LOG {message}"),
                R::Telemetry { .. } => {}
            }
        }
        sim.print_timeline();
        sim.agents
    });

    let outcome = run_workflow(WorkflowRunParams {
        script,
        args,
        journal: Journal::new(None),
        host_tx,
        cancel: tokio_util::sync::CancellationToken::new(),
        max_ops: 10_000_000,
        clock: Some(Arc::new(move || script_clock.load(Ordering::SeqCst))),
    });
    let agents = host.join().expect("host thread");

    match outcome {
        WorkflowOutcome::Completed { result } => {
            println!("COMPLETED agents={agents} {result}");
        }
        WorkflowOutcome::Paused { kind, message } => {
            println!("PAUSED ({kind:?}) agents={agents} {message}");
        }
        WorkflowOutcome::Failed { error } => {
            println!("FAILED agents={agents} {error}");
            std::process::exit(2);
        }
        other => {
            println!("{other:?} agents={agents}");
            std::process::exit(2);
        }
    }
}
