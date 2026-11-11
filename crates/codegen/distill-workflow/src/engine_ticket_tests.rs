// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for `sh()`, `spawn_agent()`, `spawn_sh()`, `wait_any()`, `wait_all()` and `cancel()`.
//!
//! The host here never answers a ticket on its own: it holds the reply until the script blocks (`AwaitTickets`) and
//! then answers what the test dictates. Completion order and timeouts are therefore exact, with no sleeps.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::host::BudgetState;

enum HeldReply {
    Agent(oneshot::Sender<Result<AgentResult, HostError>>),
    Sh(oneshot::Sender<Result<ShResult, HostError>>),
}

/// A ticketed host call the test has not answered yet.
struct Held {
    ticket: TicketHandle,
    /// The agent prompt or the shell command.
    name: String,
    /// The host clock when the call arrived.
    started_at: u64,
    reply: HeldReply,
}

impl Held {
    fn finish(self) {
        match self.reply {
            HeldReply::Agent(reply) => {
                let _ = reply.send(Ok(agent_ok(&self.name)));
            }
            HeldReply::Sh(reply) => {
                let _ = reply.send(Ok(sh_ok(&self.name)));
            }
        }
    }

    fn fail(self, error: HostError) {
        match self.reply {
            HeldReply::Agent(reply) => {
                let _ = reply.send(Err(error));
            }
            HeldReply::Sh(reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

fn agent_ok(prompt: &str) -> AgentResult {
    AgentResult {
        agent_id: format!("agent-{prompt}"),
        success: true,
        output: serde_json::Value::String(format!("ok:{prompt}")),
        cancelled: false,
        tokens_used: 10,
        duration_ms: 5,
    }
}

fn sh_ok(command: &str) -> ShResult {
    ShResult {
        ok: true,
        code: Some(0),
        stdout: format!("ran:{command}"),
        duration_ms: 5,
        ..Default::default()
    }
}

/// Removes the held call whose prompt or command is `name`.
fn take(held: &mut Vec<Held>, name: &str) -> Held {
    let position = held
        .iter()
        .position(|call| call.name == name)
        .unwrap_or_else(|| panic!("the host holds no call named {name:?}"));
    held.remove(position)
}

/// Answers every ticket the script is blocked on, successfully.
fn finish_awaited(held: &mut Vec<Held>, tickets: &[String], _all: bool, _: Option<WaitTimeout>) {
    for id in tickets {
        if let Some(position) = held.iter().position(|call| &call.ticket.id == id) {
            held.remove(position).finish();
        }
    }
}

#[derive(Clone)]
struct Budget {
    used: Arc<AtomicU64>,
    limit: u64,
}

impl Budget {
    fn unlimited() -> Self {
        Self::capped(u64::MAX)
    }

    fn capped(limit: u64) -> Self {
        Self {
            used: Arc::new(AtomicU64::new(0)),
            limit,
        }
    }

    fn used(&self) -> u64 {
        self.used.load(Ordering::SeqCst)
    }
}

struct Run {
    outcome: WorkflowOutcome,
    /// Prompts and commands the host was really asked to run, in arrival order.
    started: Vec<String>,
}

impl Run {
    fn completed(&self) -> &serde_json::Value {
        match &self.outcome {
            WorkflowOutcome::Completed { result } => result,
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}

type OnWait = dyn FnMut(&mut Vec<Held>, &[String], bool, Option<WaitTimeout>) + Send;

fn run(script: &str, journal: Journal, budget: &Budget, on_wait: Box<OnWait>) -> Run {
    run_with_clock(
        script,
        journal,
        budget,
        Arc::new(AtomicU64::new(0)),
        on_wait,
    )
}

fn run_with_clock(
    script: &str,
    journal: Journal,
    budget: &Budget,
    clock: Arc<AtomicU64>,
    mut on_wait: Box<OnWait>,
) -> Run {
    let (host_tx, mut host_rx) = mpsc::unbounded_channel();
    let started = Arc::new(Mutex::new(Vec::new()));
    let started_in_host = started.clone();
    let budget = budget.clone();
    let host_clock = clock.clone();
    let host = std::thread::spawn(move || {
        use WorkflowHostRequest as R;
        let clock = host_clock;
        let mut held: Vec<Held> = Vec::new();
        while let Some(request) = host_rx.blocking_recv() {
            match request {
                R::ReserveAgentCalls { count, reply } => {
                    let requested = budget.used().saturating_add(count);
                    if requested > budget.limit {
                        let _ = reply.send(Err(HostError::AgentCallQuotaExceeded {
                            requested,
                            maximum: budget.limit,
                        }));
                    } else {
                        budget.used.store(requested, Ordering::SeqCst);
                        let _ = reply.send(Ok(()));
                    }
                }
                R::ReleaseAgentCalls { count, reply } => {
                    budget
                        .used
                        .store(budget.used().saturating_sub(count), Ordering::SeqCst);
                    let _ = reply.send(Ok(()));
                }
                R::SpawnAgent {
                    opts,
                    ticket,
                    reply,
                } => {
                    started_in_host.lock().unwrap().push(opts.prompt.clone());
                    match ticket {
                        Some(ticket) => held.push(Held {
                            ticket,
                            name: opts.prompt,
                            started_at: clock.load(Ordering::SeqCst),
                            reply: HeldReply::Agent(reply),
                        }),
                        None => {
                            let _ = reply.send(Ok(agent_ok(&opts.prompt)));
                        }
                    }
                }
                R::RunCommand {
                    command,
                    ticket,
                    reply,
                    ..
                } => {
                    started_in_host.lock().unwrap().push(command.clone());
                    match ticket {
                        Some(ticket) => held.push(Held {
                            ticket,
                            name: command,
                            started_at: clock.load(Ordering::SeqCst),
                            reply: HeldReply::Sh(reply),
                        }),
                        None => {
                            let _ = reply.send(Ok(sh_ok(&command)));
                        }
                    }
                }
                R::AwaitTickets {
                    tickets,
                    all,
                    timeout,
                } => on_wait(&mut held, &tickets, all, timeout),
                R::BudgetQuery { reply } => {
                    let _ = reply.send(Ok(BudgetState {
                        total: None,
                        spent: budget.used(),
                        reserved: 0,
                        remaining: None,
                    }));
                }
                R::Phase { .. } | R::Log { .. } | R::Telemetry { .. } => {}
                other => panic!("unexpected host request: {other:?}"),
            }
        }
    });
    let script_clock = clock.clone();
    let outcome = run_workflow(WorkflowRunParams {
        script: script.to_string(),
        args: serde_json::json!({ "rooms": ["sala", "quarto", "cozinha"] }),
        journal,
        host_tx,
        cancel: CancellationToken::new(),
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
        clock: Some(Arc::new(move || script_clock.load(Ordering::SeqCst))),
    });
    host.join().expect("host thread");
    let started = started.lock().unwrap().clone();
    Run { outcome, started }
}

/// A host that must not be asked to run or wait for anything: the journal has to answer every call.
fn replay(script: &str, journal: Journal) -> Run {
    run(
        script,
        journal,
        &Budget::unlimited(),
        Box::new(|_, tickets, _, _| panic!("replay must not block on tickets {tickets:?}")),
    )
}

fn journal_kinds(journal: &Journal) -> Vec<String> {
    (0..journal.len() as u64)
        .filter_map(|seq| journal.entry(seq))
        .map(|entry| entry.kind.clone())
        .collect()
}

fn temp_journal() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    (dir, path)
}

const THREE_TICKETS: &str = r#"
    let meta = #{ name: "t", description: "d" };
    let tickets = [spawn_agent(#{ prompt: "a" }), spawn_agent(#{ prompt: "b" }), spawn_agent(#{ prompt: "c" })];
    let order = [];
    let done = wait_any(tickets);
    while done != () {
        order.push(done.ticket + "=" + done.result.output);
        done = wait_any(tickets);
    }
    complete(order);
"#;

/// A scheduler advances whichever pipeline finished, so `wait_any` has to hand tickets back in the order the work
/// finished, not the order it was started; and a resumed run has to see that same order again, because the
/// script's later decisions were taken on it.
#[test]
fn wait_any_returns_tickets_in_completion_order_and_replays_it() {
    let (_dir, path) = temp_journal();
    let mut finish_order = vec!["c", "a", "b"].into_iter();
    let first = run(
        THREE_TICKETS,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        Box::new(move |held, _, all, _| {
            assert!(!all, "wait_any asks for one ticket");
            take(held, finish_order.next().expect("three waits")).finish();
        }),
    );
    assert_eq!(
        first.completed(),
        &serde_json::json!(["t3=ok:c", "t1=ok:a", "t2=ok:b"])
    );
    assert_eq!(first.started, ["a", "b", "c"]);

    let second = replay(THREE_TICKETS, Journal::load(path).unwrap());
    assert_eq!(second.completed(), first.completed());
    assert!(
        second.started.is_empty(),
        "a finished ticket must come from the journal, not from a second agent: {:?}",
        second.started
    );
}

/// The case the journal design exists for. The run stops while `b` has been returned, `a` has finished without the
/// script having waited on it yet, and `c` is still running. On resume `a` and `b` cost a full agent each if they
/// run again, and the script must be told `b` first again or it replays a different history than it journaled.
#[test]
fn interrupted_run_resumes_without_rerunning_finished_tickets() {
    let (_dir, path) = temp_journal();
    let budget = Budget::unlimited();
    let mut waits = 0;
    let first = run(
        THREE_TICKETS,
        Journal::new(Some(path.clone())),
        &budget,
        Box::new(move |held, _, _, _| {
            waits += 1;
            if waits == 1 {
                take(held, "b").finish();
            } else {
                // `a` finishes in the same instant the run is cancelled under `c`.
                take(held, "a").finish();
                take(held, "c").fail(HostError::Cancelled);
            }
        }),
    );
    assert!(
        matches!(first.outcome, WorkflowOutcome::Cancelled),
        "{:?}",
        first.outcome
    );
    assert_eq!(
        budget.used(),
        2,
        "the cancelled ticket must give its agent call back; the two finished ones stay paid"
    );
    let journal = Journal::load(path.clone()).unwrap();
    assert_eq!(
        journal_kinds(&journal),
        ["ticket_agent", "wait_any", "ticket_agent"],
        "b's completion, the wait that returned it, then a's completion nobody waited on"
    );
    assert_eq!(
        journal.agent_reservation_count(),
        2,
        "resume reconciles the spent budget from the journal, which must count finished tickets"
    );

    let second = run(THREE_TICKETS, journal, &budget, Box::new(finish_awaited));
    assert_eq!(
        second.completed(),
        &serde_json::json!(["t2=ok:b", "t1=ok:a", "t3=ok:c"]),
        "b first as recorded, then a from the journal, then the re-run c"
    );
    assert_eq!(
        second.started,
        ["c"],
        "only the cancelled ticket runs again"
    );
    assert_eq!(budget.used(), 3, "c is paid for once");

    let third = replay(THREE_TICKETS, Journal::load(path).unwrap());
    assert_eq!(third.completed(), second.completed());
}

const QUORUM: &str = r#"
    let meta = #{ name: "t", description: "d" };
    let slow = spawn_agent(#{ prompt: "slow" });
    let first = wait_any([slow], 5000);
    let second = wait_any([slow]);
    complete(#{ first: first, late: second.result.output, late_after: second.elapsed_ms });
"#;

/// A quorum ("go on with what we have after 5 s") is a decision taken on a timeout. By the time the run is resumed
/// the slow ticket's result is in the journal, so only a recorded timeout keeps the script on the branch it took.
#[test]
fn wait_any_timeout_is_journaled_and_replays_as_a_timeout() {
    let (_dir, path) = temp_journal();
    let clock = Arc::new(AtomicU64::new(0));
    let host_clock = clock.clone();
    let first = run_with_clock(
        QUORUM,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        clock,
        Box::new(move |held, tickets, _, timeout| match timeout {
            Some(timeout) => {
                assert_eq!(timeout.ms, 5000, "the host is told how long to wait");
                assert_eq!(tickets, ["t1"]);
                host_clock.fetch_add(timeout.ms, Ordering::SeqCst);
                let _ = timeout.fire.send(());
            }
            None => {
                host_clock.fetch_add(1200, Ordering::SeqCst);
                take(held, "slow").finish();
            }
        }),
    );
    // A timeout is a map, not unit: unit is reserved for "none of these tickets is left to hand out".
    let expected = serde_json::json!({
        "first": { "timeout": true, "elapsed_ms": 5000 },
        "late": "ok:slow",
        "late_after": 1200,
    });
    assert_eq!(first.completed(), &expected);

    let journal = Journal::load(path).unwrap();
    assert_eq!(
        journal_kinds(&journal),
        ["wait_any", "ticket_agent", "wait_any"]
    );
    assert_eq!(
        journal.entry(0).map(|entry| entry.result.clone()),
        Some(serde_json::json!({ "timeout": true, "elapsed_ms": 5000 }))
    );
    // The replaying engine's clock stands still: the durations can only come from the journal.
    let second = replay(QUORUM, journal);
    assert_eq!(second.completed(), &expected);
    assert!(second.started.is_empty());
}

/// Scripts have no clock (`timestamp()` would break replay), yet a quorum has a deadline to keep across several
/// waits. `elapsed_ms` is that clock: time counts only while the script is parked in the call, and a ticket that
/// had finished before the call costs nothing. Among finished tickets the lowest one wins, whichever replied
/// first, so the pick does not depend on the instant the engine happened to look.
#[test]
fn wait_any_counts_blocked_time_and_returns_finished_tickets_lowest_first() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let a = spawn_agent(#{ prompt: "a" });
        let b = spawn_agent(#{ prompt: "b" });
        let c = spawn_agent(#{ prompt: "c" });
        let waits = [wait_any([c]), wait_any([a, b]), wait_any([a, b]), wait_any([a, b])];
        complete(waits.map(|w| if w == () { "nothing left" } else { w.ticket + "@" + w.elapsed_ms }));
    "#;
    let (_dir, path) = temp_journal();
    let clock = Arc::new(AtomicU64::new(0));
    let host_clock = clock.clone();
    let mut waits = 0;
    let first = run_with_clock(
        script,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        clock,
        Box::new(move |held, tickets, _, _| {
            waits += 1;
            assert_eq!(waits, 1, "only the first wait has to block: {tickets:?}");
            // b finishes before a, and both before the ticket the script is waiting for.
            host_clock.store(700, Ordering::SeqCst);
            take(held, "b").finish();
            take(held, "a").finish();
            take(held, "c").finish();
        }),
    );
    let expected = serde_json::json!(["t3@700", "t1@0", "t2@0", "nothing left"]);
    assert_eq!(first.completed(), &expected);
    assert_eq!(
        replay(script, Journal::load(path).unwrap()).completed(),
        &expected
    );
}

/// `wait_any(tickets, 0)` is how a script polls. It must answer from what has already arrived, without parking the
/// script on the host's clock.
#[test]
fn wait_any_zero_timeout_polls_without_blocking() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let t = spawn_agent(#{ prompt: "pending" });
        let polled = wait_any([t], 0);
        let waited = wait_any([t]);
        complete(#{ polled: polled, waited: waited.ticket });
    "#;
    let mut blocking_waits = 0;
    let outcome = run(
        script,
        Journal::new(None),
        &Budget::unlimited(),
        Box::new(move |held, _, _, timeout| {
            blocking_waits += 1;
            assert_eq!(blocking_waits, 1, "only the wait without timeout may block");
            assert!(timeout.is_none());
            take(held, "pending").finish();
        }),
    );
    assert_eq!(
        outcome.completed(),
        &serde_json::json!({ "polled": { "timeout": true, "elapsed_ms": 0 }, "waited": "t1" })
    );
}

const BUDGETED: &str = r#"
    let meta = #{ name: "t", description: "d" };
    let a = spawn_agent(#{ prompt: "a" });
    let b = spawn_agent(#{ prompt: "b" });
    let c = spawn_agent(#{ prompt: "c" });
    let results = wait_all([a, b, c]);
    complete(results.map(|r| r.output));
"#;

/// Hitting the agent budget stops the run so the user can raise the cap and resume. The agents already running are
/// paid for: they must finish, be journaled, and not be started (and charged) a second time after the raise.
#[test]
fn budget_exceeded_spawn_keeps_running_tickets_for_the_raised_cap_resume() {
    let (_dir, path) = temp_journal();
    let mut budget = Budget::capped(2);
    let first = run(
        BUDGETED,
        Journal::new(Some(path.clone())),
        &budget,
        Box::new(|held, tickets, all, timeout| {
            assert!(
                all,
                "the drain at script end waits for every running ticket"
            );
            finish_awaited(held, tickets, all, timeout);
        }),
    );
    assert!(
        matches!(first.outcome, WorkflowOutcome::BudgetExceeded { .. }),
        "{:?}",
        first.outcome
    );
    assert_eq!(first.started, ["a", "b"], "the third spawn was refused");
    let journal = Journal::load(path).unwrap();
    assert_eq!(journal_kinds(&journal), ["ticket_agent", "ticket_agent"]);
    assert_eq!(journal.agent_reservation_count(), budget.used());

    budget.limit = 3;
    let second = run(BUDGETED, journal, &budget, Box::new(finish_awaited));
    assert_eq!(
        second.completed(),
        &serde_json::json!(["ok:a", "ok:b", "ok:c"])
    );
    assert_eq!(second.started, ["c"]);
    assert_eq!(budget.used(), 3, "three agents ran, three were charged");
}

/// A paused or stopped run cancels its children. Nothing finished, so nothing may stay charged or journaled: the
/// resumed run starts both again and pays for each exactly once.
#[test]
fn cancelled_tickets_release_their_budget_and_run_again_on_resume() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let results = wait_all([spawn_agent(#{ prompt: "a" }), spawn_agent(#{ prompt: "b" })]);
        complete(results.map(|r| r.output));
    "#;
    let (_dir, path) = temp_journal();
    let budget = Budget::unlimited();
    let first = run(
        script,
        Journal::new(Some(path.clone())),
        &budget,
        Box::new(|held, _, _, _| {
            for call in held.drain(..) {
                call.fail(HostError::Cancelled);
            }
        }),
    );
    assert!(matches!(first.outcome, WorkflowOutcome::Cancelled));
    assert_eq!(budget.used(), 0);
    let journal = Journal::load(path).unwrap();
    assert!(journal.is_empty(), "{:?}", journal_kinds(&journal));

    let second = run(script, journal, &budget, Box::new(finish_awaited));
    assert_eq!(second.completed(), &serde_json::json!(["ok:a", "ok:b"]));
    assert_eq!(budget.used(), 2);
}

/// `sh()` exists so a deterministic step does not cost an agent. A command that exits non-zero is an answer the
/// script reads, not an error it has to catch; and a resumed run must not run the command (and its side effects)
/// again.
#[test]
fn sh_returns_a_failed_exit_as_data_and_replays_without_rerunning() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let r = sh("bun check.ts", #{ timeout_ms: 2000, label: "check", cwd: "tools" });
        complete(#{ ok: r.ok, code: r.code, out: r.stdout, err: r.stderr, timed_out: r.timed_out });
    "#;
    let (_dir, path) = temp_journal();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let host = std::thread::spawn(move || {
        let mut seen = Vec::new();
        while let Some(request) = rx.blocking_recv() {
            if let WorkflowHostRequest::RunCommand {
                command,
                opts,
                ticket,
                reply,
            } = request
            {
                assert!(ticket.is_none(), "sh() blocks; it holds no ticket");
                seen.push((command, opts));
                let _ = reply.send(Ok(ShResult {
                    ok: false,
                    code: Some(3),
                    stdout: "partial".into(),
                    stderr: "boom".into(),
                    duration_ms: 12,
                    ..Default::default()
                }));
            }
        }
        seen
    });
    let outcome = run_workflow(WorkflowRunParams {
        script: script.to_string(),
        args: serde_json::Value::Null,
        journal: Journal::new(Some(path.clone())),
        host_tx: tx,
        cancel: CancellationToken::new(),
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
        clock: None,
    });
    let seen = host.join().unwrap();
    assert_eq!(
        seen,
        [(
            "bun check.ts".to_string(),
            ShOpts {
                timeout_ms: Some(2000),
                label: Some("check".into()),
                cwd: Some("tools".into()),
            }
        )]
    );
    let expected = serde_json::json!({
        "ok": false, "code": 3, "out": "partial", "err": "boom", "timed_out": false
    });
    match outcome {
        WorkflowOutcome::Completed { result } => assert_eq!(result, expected),
        other => panic!("a non-zero exit must not fail the script: {other:?}"),
    }

    let journal = Journal::load(path).unwrap();
    assert_eq!(journal_kinds(&journal), ["sh"]);
    let second = replay(script, journal);
    assert_eq!(second.completed(), &expected);
    assert!(second.started.is_empty(), "the command ran again on replay");
}

/// The host refuses `sh()` when the session's permissions do not allow unattended commands. The script must be able
/// to fall back (say, to an agent), and a resumed run must take the same fallback rather than suddenly run the command.
#[test]
fn sh_host_refusal_is_catchable_and_replays() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let how = "";
        try { sh("bun render.ts"); how = "sh"; } catch (e) { how = "refused: " + e; }
        complete(how);
    "#;
    let (_dir, path) = temp_journal();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let host = std::thread::spawn(move || {
        while let Some(request) = rx.blocking_recv() {
            if let WorkflowHostRequest::RunCommand { reply, .. } = request {
                let _ = reply.send(Err(HostError::Unsupported("needs always-approve".into())));
            }
        }
    });
    let outcome = run_workflow(WorkflowRunParams {
        script: script.to_string(),
        args: serde_json::Value::Null,
        journal: Journal::new(Some(path.clone())),
        host_tx: tx,
        cancel: CancellationToken::new(),
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
        clock: None,
    });
    host.join().unwrap();
    let expected = serde_json::json!("refused: needs always-approve");
    match outcome {
        WorkflowOutcome::Completed { result } => assert_eq!(result, expected),
        other => panic!("expected Completed, got {other:?}"),
    }
    let second = replay(script, Journal::load(path).unwrap());
    assert_eq!(second.completed(), &expected);
}

/// `wait_all` is the final collection of a fan-out. Its results line up with the tickets the script passed, so the
/// script can zip them with its own work list whatever order the work finished in. A ticket whose host call failed
/// is `()`, the same contract as a failed `parallel()` slot.
#[test]
fn wait_all_returns_results_in_ticket_order_with_failures_as_unit() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let tickets = [
            spawn_sh("first"),
            spawn_agent(#{ prompt: "second" }),
            spawn_sh("third", #{ label: "3" }),
        ];
        let results = wait_all(tickets);
        complete([results[0].stdout, results[1].output, results[2] == ()]);
    "#;
    let (_dir, path) = temp_journal();
    let first = run(
        script,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        Box::new(|held, _, all, _| {
            assert!(all);
            take(held, "third").fail(HostError::Failed("spawn failed".into()));
            take(held, "second").finish();
            take(held, "first").finish();
        }),
    );
    let expected = serde_json::json!(["ran:first", "ok:second", true]);
    assert_eq!(first.completed(), &expected);
    let journal = Journal::load(path).unwrap();
    assert_eq!(
        journal.agent_reservation_count(),
        1,
        "shell commands do not spend the agent budget"
    );
    assert_eq!(replay(script, journal).completed(), &expected);
}

/// A host failure on a ticket must reach the script with its reason: a refused `spawn_sh` that looked like an empty
/// success would let a pipeline go on without the file the command was supposed to produce.
#[test]
fn failed_ticket_reports_its_error_through_wait_any() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let done = wait_any([spawn_sh("bun render.ts")]);
        complete(#{ ticket: done.ticket, failed: done.result == (), error: done.error, ms: done.elapsed_ms });
    "#;
    let outcome = run(
        script,
        Journal::new(None),
        &Budget::unlimited(),
        Box::new(|held, _, _, _| {
            take(held, "bun render.ts").fail(HostError::Unsupported("needs always-approve".into()));
        }),
    );
    assert_eq!(
        outcome.completed(),
        &serde_json::json!({ "ticket": "t1", "failed": true, "error": "needs always-approve", "ms": 0 })
    );
}

/// After a quorum the script stops the stragglers instead of paying for them to the end. The host is told through
/// the ticket's own token, and the ticket still resolves, marked cancelled, so `wait_any` loops terminate.
#[test]
fn cancel_reaches_the_host_and_the_ticket_resolves_cancelled() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let keep = spawn_agent(#{ prompt: "keep" });
        let straggler = spawn_agent(#{ prompt: "straggler" });
        let first = wait_any([keep, straggler]);
        cancel(straggler);
        let second = wait_any([keep, straggler]);
        complete([first.result.output, second.ticket, second.result.cancelled]);
    "#;
    let outcome = run(
        script,
        Journal::new(None),
        &Budget::unlimited(),
        Box::new(|held, tickets, _, _| {
            if held.len() == 2 {
                assert!(held.iter().all(|call| !call.ticket.cancel.is_cancelled()));
                take(held, "keep").finish();
                return;
            }
            assert_eq!(
                tickets,
                ["t2"],
                "the returned ticket is not waited on again"
            );
            let straggler = take(held, "straggler");
            assert!(
                straggler.ticket.cancel.is_cancelled(),
                "cancel(ticket) must reach the host"
            );
            let HeldReply::Agent(reply) = straggler.reply else {
                panic!("an agent ticket");
            };
            let _ = reply.send(Ok(AgentResult {
                success: false,
                cancelled: true,
                ..agent_ok("straggler")
            }));
        }),
    );
    assert_eq!(
        outcome.completed(),
        &serde_json::json!(["ok:keep", "t2", true])
    );
}

/// A script may end while tickets it never waited on are still running. They were paid for and may be editing the
/// workspace, so the run waits for them and journals what they returned; a script that pauses with `await_user`
/// then gets those results back on resume instead of starting the agents over.
#[test]
fn unwaited_tickets_are_drained_and_resolve_from_the_journal_after_a_pause() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let background = spawn_agent(#{ prompt: "background" });
        await_user("user", "check the draft");
        let done = wait_any([background]);
        complete(done.result.output);
    "#;
    let (_dir, path) = temp_journal();
    let first = run(
        script,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        Box::new(|held, tickets, all, timeout| {
            assert!(all, "the script is not waiting; this is the drain");
            finish_awaited(held, tickets, all, timeout);
        }),
    );
    assert!(
        matches!(first.outcome, WorkflowOutcome::Paused { .. }),
        "{:?}",
        first.outcome
    );
    let journal = Journal::load(path).unwrap();
    assert_eq!(journal_kinds(&journal), ["await_user", "ticket_agent"]);

    let second = run(
        script,
        journal,
        &Budget::unlimited(),
        Box::new(|_, tickets, _, _| panic!("nothing is running after the drain: {tickets:?}")),
    );
    assert_eq!(second.completed(), &serde_json::json!("ok:background"));
    assert!(second.started.is_empty(), "{:?}", second.started);
}

/// Journals written before tickets existed, and scripts that never use them, must replay exactly as before: same
/// entry kinds at the same positions, and no new request reaching the host.
#[test]
fn scripts_without_tickets_keep_their_journal_layout() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let one = agent("one");
        let panel = parallel([#{ prompt: "two" }, #{ prompt: "three" }]);
        let spent = budget();
        complete([one.output, panel[0].output, panel[1].output]);
    "#;
    let (_dir, path) = temp_journal();
    let first = run(
        script,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        Box::new(|_, _, _, _| panic!("a script without tickets never blocks on any")),
    );
    let expected = serde_json::json!(["ok:one", "ok:two", "ok:three"]);
    assert_eq!(first.completed(), &expected);
    let journal = Journal::load(path).unwrap();
    assert_eq!(
        journal_kinds(&journal),
        ["spawn_agent", "spawn_agent", "spawn_agent", "budget"]
    );
    assert_eq!(journal.agent_reservation_count(), 3);
    let second = replay(script, journal);
    assert_eq!(second.completed(), &expected);
    assert!(second.started.is_empty());
}

/// A run that dies on a host error is resumable: the failing call is pruned from the journal's end and retried. With
/// tickets, their drained results are written after that error; if they buried it, the run would replay the
/// failure forever and the finished rooms would be lost with it.
#[test]
fn failing_host_call_stays_retryable_when_tickets_drain_after_it() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let room = spawn_agent(#{ prompt: "room" });
        let rendered = sh("bun render.ts");
        let done = wait_any([room]);
        complete([rendered.stdout, done.result.output]);
    "#;
    let (_dir, path) = temp_journal();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let host = std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Some(request) = rx.blocking_recv() {
            match request {
                WorkflowHostRequest::ReserveAgentCalls { reply, .. } => {
                    let _ = reply.send(Ok(()));
                }
                WorkflowHostRequest::SpawnAgent { reply, .. } => held.push(reply),
                WorkflowHostRequest::RunCommand { reply, .. } => {
                    let _ = reply.send(Err(HostError::Failed("bun: command not found".into())));
                }
                WorkflowHostRequest::AwaitTickets { .. } => {
                    for reply in held.drain(..) {
                        let _ = reply.send(Ok(agent_ok("room")));
                    }
                }
                other => panic!("unexpected host request: {other:?}"),
            }
        }
    });
    let outcome = run_workflow(WorkflowRunParams {
        script: script.to_string(),
        args: serde_json::Value::Null,
        journal: Journal::new(Some(path.clone())),
        host_tx: tx,
        cancel: CancellationToken::new(),
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
        clock: None,
    });
    host.join().unwrap();
    let WorkflowOutcome::Failed { error } = outcome else {
        panic!("an uncaught host failure fails the run: {outcome:?}");
    };
    assert!(error.contains("bun: command not found"), "{error}");

    let mut journal = Journal::load(path).unwrap();
    assert_eq!(
        journal_kinds(&journal),
        ["ticket_agent"],
        "the failing sh() entry is gone, the finished room is kept"
    );
    assert!(
        !journal.prune_trailing_host_error(&error).unwrap(),
        "nothing is left for the resume-time prune to remove"
    );

    let second = run(
        script,
        journal,
        &Budget::unlimited(),
        Box::new(|_, tickets, _, _| panic!("the room is already finished: {tickets:?}")),
    );
    assert_eq!(
        second.completed(),
        &serde_json::json!(["ran:bun render.ts", "ok:room"])
    );
    assert_eq!(
        second.started,
        ["bun render.ts"],
        "the command is retried, the room is not run again"
    );
}

/// A ticket id is a position, so the journal entry it finds must be for the same request. An edited script that
/// spawns something else under that id would otherwise be handed another agent's result.
#[test]
fn recorded_ticket_for_a_different_request_is_a_divergence() {
    let (_dir, path) = temp_journal();
    let first = run(
        r#"
        let meta = #{ name: "t", description: "d" };
        complete(wait_all([spawn_agent(#{ prompt: "original" })]));
        "#,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        Box::new(finish_awaited),
    );
    assert!(matches!(first.outcome, WorkflowOutcome::Completed { .. }));

    let second = run(
        r#"
        let meta = #{ name: "t", description: "d" };
        complete(wait_all([spawn_agent(#{ prompt: "edited" })]));
        "#,
        Journal::load(path).unwrap(),
        &Budget::unlimited(),
        Box::new(finish_awaited),
    );
    match second.outcome {
        WorkflowOutcome::Failed { error } => assert!(error.contains("divergence"), "{error}"),
        other => panic!("expected a divergence failure, got {other:?}"),
    }
    assert!(
        second.started.is_empty(),
        "a divergent replay must not run anything"
    );
}

/// Workflows are saved once and run on whatever Distill is installed. A script asks "does this engine have tickets?"
/// with `wait_all([])` inside try/catch and falls back to `parallel()` when it does not. That only works if the
/// probe is free on an engine that has them (no journal entry, no host request, or the two branches would replay
/// differently) and if every ticket function is a plain function name, which an older engine parses and rejects
/// only when called, where the catch sees it.
#[test]
fn wait_all_of_nothing_is_a_free_feature_probe() {
    let script = r#"
        let meta = #{ name: "t", description: "d" };
        let has_tickets = false;
        try { wait_all([]); has_tickets = true; } catch (e) {}
        // What the probe does on an engine without the functions: an unknown function is an ordinary error.
        let older = "has it";
        try { function_this_engine_lacks([]); } catch (e) { older = "falls back"; }
        complete(#{ has_tickets: has_tickets, older: older, empty: wait_all([]) });
    "#;
    let (_dir, path) = temp_journal();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(WorkflowRunParams {
        script: script.to_string(),
        args: serde_json::Value::Null,
        journal: Journal::new(Some(path.clone())),
        host_tx: tx,
        cancel: CancellationToken::new(),
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
        clock: None,
    });
    match outcome {
        WorkflowOutcome::Completed { result } => assert_eq!(
            result,
            serde_json::json!({ "has_tickets": true, "older": "falls back", "empty": [] })
        ),
        other => panic!("the probe must not fail the script: {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "the probe must not reach the host");
    assert!(
        Journal::load(path).unwrap().is_empty(),
        "the probe must not take a journal position"
    );
}

#[test]
fn misuse_is_reported_to_the_script() {
    let expect_error = |body: &str, expected: &str| {
        let script = format!("let meta = #{{ name: \"t\", description: \"d\" }};\n{body}");
        let outcome = run(
            &script,
            Journal::new(None),
            &Budget::unlimited(),
            Box::new(finish_awaited),
        )
        .outcome;
        match outcome {
            WorkflowOutcome::Failed { error } => {
                assert!(error.contains(expected), "{body}: {error}")
            }
            other => panic!("{body}: expected Failed, got {other:?}"),
        }
    };
    expect_error(r#"wait_any(["t9"]);"#, "unknown ticket");
    expect_error(r#"wait_all([42]);"#, "tickets returned by spawn_agent()");
    expect_error(r#"cancel("nope");"#, "unknown ticket");
    expect_error(
        r#"spawn_agent("just a prompt");"#,
        "Function not found: spawn_agent",
    );
    expect_error(
        r#"spawn_agent(#{ label: "no prompt" });"#,
        "prompt must not be empty",
    );
    expect_error(r#"sh("   ");"#, "must not be empty");
    expect_error(r#"sh("ls", #{ timeout: 5 });"#, "invalid sh options");
    expect_error(r#"sh("ls", #{ timeout_ms: 0 });"#, "greater than zero");
    expect_error(
        r#"let t = spawn_agent(#{ prompt: "p" }); wait_any([t], -1);"#,
        "must not be negative",
    );
}

/// The reason tickets exist: rooms are independent pipelines, and with one `parallel()` per stage every room waits
/// for the slowest room at every stage. Here each room is `plan -> 2 drafts -> render (shell) -> review`, the rooms
/// take different times, and the host keeps a virtual clock. Driven by tickets the run takes as long as its slowest
/// room; stage barriers would take the sum of each stage's slowest room.
const ROOMS: &str = r#"
    let meta = #{ name: "rooms", description: "one pipeline per room" };
    let rooms = args.rooms;
    let owner = #{};
    let state = [];
    let open = [];
    for i in 0..rooms.len() {
        let t = spawn_agent(#{ prompt: rooms[i] + ":plan" });
        owner[t] = i;
        open.push(t);
        state.push(#{ stage: "plan", left: 1, log: [] });
    }
    let done = wait_any(open);
    while done != () {
        let i = owner[done.ticket];
        let room = state[i];
        room.left -= 1;
        if room.left == 0 {
            room.log.push(room.stage);
            let next = [];
            if room.stage == "plan" {
                room.stage = "draft";
                next.push(spawn_agent(#{ prompt: rooms[i] + ":draft-a" }));
                next.push(spawn_agent(#{ prompt: rooms[i] + ":draft-b" }));
            } else if room.stage == "draft" {
                room.stage = "render";
                next.push(spawn_sh(rooms[i] + ":render"));
            } else if room.stage == "render" {
                room.stage = "review";
                next.push(spawn_agent(#{ prompt: rooms[i] + ":review" }));
            } else {
                room.stage = "done";
            }
            room.left = next.len();
            for t in next { owner[t] = i; open.push(t); }
        }
        state[i] = room;
        done = wait_any(open);
    }
    complete(state.map(|room| room.stage + ":" + room.log.len()));
"#;

/// Virtual milliseconds one step of a room takes: the room sets the pace, so `sala` is always the slowest.
fn step_ms(name: &str) -> u64 {
    let pace = match name.split(':').next() {
        Some("sala") => 9,
        Some("quarto") => 4,
        _ => 1,
    };
    let stage = match name.split(':').nth(1) {
        Some("plan") => 100,
        Some("draft-a") => 300,
        Some("draft-b") => 200,
        Some("render") => 10,
        _ => 50,
    };
    pace * stage
}

#[test]
fn room_pipelines_advance_independently_and_replay_identically() {
    let (_dir, path) = temp_journal();
    let clock = Arc::new(AtomicU64::new(0));
    let host_clock = clock.clone();
    let finished = Arc::new(Mutex::new(Vec::new()));
    let finished_in_host = finished.clone();
    let first = run_with_clock(
        ROOMS,
        Journal::new(Some(path.clone())),
        &Budget::unlimited(),
        clock.clone(),
        Box::new(move |held, tickets, all, _| {
            assert!(!all);
            // Discrete-event step: the awaited call that finishes first, and the clock moves to that instant.
            let next = held
                .iter()
                .filter(|call| tickets.contains(&call.ticket.id))
                .min_by_key(|call| call.started_at + step_ms(&call.name))
                .map(|call| call.name.clone())
                .expect("the script waits on something running");
            let call = take(held, &next);
            let at = call.started_at + step_ms(&call.name);
            host_clock.fetch_max(at, Ordering::SeqCst);
            finished_in_host.lock().unwrap().push(next);
            call.finish();
        }),
    );
    assert_eq!(
        first.completed(),
        &serde_json::json!(["done:4", "done:4", "done:4"])
    );

    let finished = finished.lock().unwrap().clone();
    let position = |name: &str| finished.iter().position(|n| n == name).unwrap();
    assert!(
        position("cozinha:review") < position("sala:plan"),
        "the fast room must run its whole pipeline while the slow room is still planning: {finished:?}"
    );
    // sala: plan 900 + slowest draft 2700 + render 90 + review 450.
    assert_eq!(
        clock.load(Ordering::SeqCst),
        4140,
        "the run lasts as long as its slowest room, not the sum of every stage's slowest room"
    );
    assert_eq!(
        first.started.len(),
        15,
        "3 rooms x (plan, 2 drafts, render, review)"
    );

    let second = replay(ROOMS, Journal::load(path).unwrap());
    assert_eq!(second.completed(), first.completed());
    assert!(second.started.is_empty(), "{:?}", second.started);
}
