// Modified for Distill by Samuel Fajreldines, 2026.
//! Integration coverage for the planner trigger inside `setup_goal` and the session-load hook `maybe_reconcile_active_goal_without_plan`.
//! Uses the same single-thread runtime and LocalSet pattern as the verification-stage e2e suite.

use super::support::*;
use super::*;
use distill_tools::implementations::distill::task::types::{
    SubagentCancelTarget, SubagentEvent, SubagentResult, SubagentSpawnRequest,
};
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering as SeqOrd};
use tempfile::TempDir;

/// Pull the planner's plan-file path from the prompt by its backtick-quoted `.md` token.
/// Rewording the surrounding sentence therefore can't silently break the fake, which would otherwise write nothing and fail far from the cause.
fn plan_path_from_prompt(prompt: &str) -> Option<String> {
    // Backtick-delimited tokens sit at the odd indices of the split.
    prompt
        .split('`')
        .skip(1)
        .step_by(2)
        .find(|token| token.ends_with(".md"))
        .map(str::to_owned)
}

/// Spawn behaviour knobs for the planner-coordinator stub.
enum SpawnBehaviour {
    /// Parse `{PLAN_FILE}` out of the prompt, write `body` there, then respond `Done`.
    WritePlanThenDone { body: &'static [u8] },
    WaitForCancelsThenWrite {
        cancels: usize,
        started: tokio::sync::mpsc::UnboundedSender<usize>,
        objectives: StdArc<std::sync::Mutex<Vec<String>>>,
        body: &'static [u8],
    },
    /// Reply success but never write the file.
    NoWriteThenDone,
    /// Reply with subagent runtime failure.
    Runtime { message: String, cancelled: bool },
    /// First spawn fails with `Runtime { message, cancelled: false }`; every later spawn behaves as `WritePlanThenDone`.
    RuntimeThenWritePlan {
        message: String,
        body: &'static [u8],
    },
}

/// Captured planner spawn flags (harness-internal `SubagentRequest` fields).
#[derive(Default)]
struct PlannerSpawnCapture {
    fork_context: StdArc<std::sync::Mutex<Vec<bool>>>,
    surface_completion: StdArc<std::sync::Mutex<Vec<bool>>>,
    model: StdArc<std::sync::Mutex<Vec<Option<String>>>>,
}

/// Write `body` at the plan path the prompt names, then reply the planner's `Done`.
fn plan_written(
    req: &SubagentSpawnRequest,
    plan_path: Option<&str>,
    body: &[u8],
) -> SubagentResult {
    if let Some(p) = plan_path {
        let _ = std::fs::create_dir_all(std::path::Path::new(p).parent().unwrap());
        let _ = std::fs::write(p, body);
    }
    SubagentResult {
        success: true,
        output: StdArc::from("Done"),
        subagent_id: req.id.clone(),
        child_session_id: req.id.clone(),
        ..Default::default()
    }
}

/// Stand up a coordinator that handles exactly the spawn behaviours the planner exercises: each `Spawn` is answered on its `result_tx`.
/// Returns the sender half and a spawn-count counter the test reads at the end.
fn spawn_planner_coordinator(
    behaviour: SpawnBehaviour,
) -> (
    tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    StdArc<AtomicUsize>,
) {
    let (tx, count, _capture) = spawn_planner_coordinator_capturing(behaviour);
    (tx, count)
}

/// Like [`spawn_planner_coordinator`] but also records harness flags (`fork_context`, `surface_completion`) from each spawn.
fn spawn_planner_coordinator_capturing(
    behaviour: SpawnBehaviour,
) -> (
    tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    StdArc<AtomicUsize>,
    PlannerSpawnCapture,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    let spawn_count = StdArc::new(AtomicUsize::new(0));
    let count_task = StdArc::clone(&spawn_count);
    let capture = PlannerSpawnCapture::default();
    let fork_log = StdArc::clone(&capture.fork_context);
    let surface_log = StdArc::clone(&capture.surface_completion);
    let model_log = StdArc::clone(&capture.model);
    tokio::task::spawn_local(async move {
        while let Some(ev) = rx.recv().await {
            if let SubagentEvent::Spawn(req) = ev {
                count_task.fetch_add(1, SeqOrd::SeqCst);
                fork_log.lock().unwrap().push(req.fork_context);
                surface_log.lock().unwrap().push(req.surface_completion);
                model_log
                    .lock()
                    .unwrap()
                    .push(req.runtime_overrides.model.clone());
                let plan_path = plan_path_from_prompt(&req.prompt);
                let result = match &behaviour {
                    SpawnBehaviour::WritePlanThenDone { body } => {
                        plan_written(&req, plan_path.as_deref(), body)
                    }
                    SpawnBehaviour::WaitForCancelsThenWrite {
                        cancels,
                        started,
                        objectives,
                        body,
                    } => {
                        objectives.lock().unwrap().push(req.prompt.clone());
                        let spawn = count_task.load(SeqOrd::SeqCst);
                        let _ = started.send(spawn);
                        if spawn <= *cancels {
                            req.cancel_token.cancelled().await;
                            SubagentResult {
                                success: false,
                                error: Some("cancelled".into()),
                                cancelled: true,
                                subagent_id: req.id.clone(),
                                child_session_id: req.id.clone(),
                                ..Default::default()
                            }
                        } else {
                            plan_written(&req, plan_path.as_deref(), body)
                        }
                    }
                    SpawnBehaviour::NoWriteThenDone => SubagentResult {
                        success: true,
                        output: StdArc::from("Done"),
                        subagent_id: req.id.clone(),
                        child_session_id: req.id.clone(),
                        ..Default::default()
                    },
                    SpawnBehaviour::Runtime { message, cancelled } => SubagentResult {
                        success: false,
                        error: Some(message.clone()),
                        cancelled: *cancelled,
                        subagent_id: req.id.clone(),
                        child_session_id: req.id.clone(),
                        ..Default::default()
                    },
                    SpawnBehaviour::RuntimeThenWritePlan { message, body } => {
                        if count_task.load(SeqOrd::SeqCst) == 1 {
                            SubagentResult {
                                success: false,
                                error: Some(message.clone()),
                                subagent_id: req.id.clone(),
                                child_session_id: req.id.clone(),
                                ..Default::default()
                            }
                        } else {
                            plan_written(&req, plan_path.as_deref(), body)
                        }
                    }
                };
                let _ = req.result_tx.send(result);
            }
        }
    });
    (tx, spawn_count, capture)
}

/// Build a `SessionActor` with goal harness enabled, planner enabled, the supplied coordinator, and a unique tempdir session dir.
/// The tempdir isolates each test's `plan_path()`.
/// There is **no active goal yet**; the caller drives `setup_goal` or `create_goal` directly.
async fn make_planner_actor(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    planner_enabled: bool,
) -> (StdArc<SessionActor>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_planner_enabled = planner_enabled;
    actor.goal_tracker = Arc::new(parking_lot::Mutex::new(
        crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf()),
    ));
    if let Some(tx) = coordinator_tx {
        actor.tool_context.subagent_event_tx = Some(tx);
    }
    (StdArc::new(actor), tmp)
}

/// Like [`make_planner_actor`] but retains the persistence receiver so a test can inspect the `GoalUpdated` notifications the planner run emits.
/// Tests use it to assert the wire-only `planning` flag is set then cleared.
async fn make_planner_actor_capturing(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    planner_enabled: bool,
) -> (
    StdArc<SessionActor>,
    TempDir,
    tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let tmp = TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_planner_enabled = planner_enabled;
    actor.goal_tracker = Arc::new(parking_lot::Mutex::new(
        crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf()),
    ));
    if let Some(tx) = coordinator_tx {
        actor.tool_context.subagent_event_tx = Some(tx);
    }
    (StdArc::new(actor), tmp, persistence_rx)
}

/// Drain every persisted `GoalUpdated` notification and project to its wire-only `planning` flag, preserving emission order.
fn drain_goal_planning_flags(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) -> Vec<Option<bool>> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n)) = msg
            && let crate::extensions::notification::SessionUpdate::GoalUpdated { planning, .. } =
                n.update
        {
            out.push(planning);
        }
    }
    out
}

fn create_test_goal(actor: &SessionActor) {
    actor.goal_tracker.lock().create_goal(
        "g-test".into(),
        "test objective".into(),
        None,
        0,
        "2026-01-01T00:00:00Z".into(),
        None,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn send_now_restarts_planner_with_all_steering() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
            let objectives = StdArc::new(std::sync::Mutex::new(Vec::new()));
            let (tx, spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WaitForCancelsThenWrite {
                    cancels: 2,
                    started: started_tx,
                    objectives: StdArc::clone(&objectives),
                    body: b"# Plan\n",
                });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(user_item("goal-running", "A"));
                state.running_task = Some(running_task_stub("goal-running"));
            }
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("goal-running".into());

            let planner = {
                let actor = StdArc::clone(&actor);
                tokio::task::spawn_local(async move { actor.setup_goal("do X", None).await })
            };

            for (spawn, text) in [(1, "first"), (2, "second")] {
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(5), started_rx.recv())
                        .await
                        .expect("planner spawn"),
                    Some(spawn),
                );
                let (respond_to, response_rx) = tokio::sync::oneshot::channel();
                assert!(
                    !actor
                        .queue_input(QueueInputRequest {
                            send_now: true,
                            ..queue_input_request(
                                vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                                &format!("user-steer-{spawn}"),
                                respond_to,
                            )
                        })
                        .await
                );
                assert!(matches!(
                    response_rx.await.unwrap().unwrap().completion_kind,
                    PromptCompletionKind::RemovedFromQueue
                ));
            }

            tokio::time::timeout(std::time::Duration::from_secs(5), planner)
                .await
                .expect("planner completion")
                .unwrap();
            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 3);
            let prompts = objectives.lock().unwrap();
            let objectives = prompts
                .iter()
                .map(|prompt| {
                    prompt
                        .split_once("\n\nOBJECTIVE:\n")
                        .unwrap()
                        .1
                        .split_once("\n\nCONTEXT:\n")
                        .unwrap()
                        .0
                })
                .collect::<Vec<_>>();
            assert_eq!(
                objectives,
                [
                    "do X",
                    "do X\n\nUser steering:\nfirst",
                    "do X\n\nUser steering:\nfirst\n\nsecond",
                ]
            );

            // Staged files (`plan-<uuid>.md`, `plan-baseline-<uuid>.md`) are `TempPath`s
            // Interrupted attempts drop theirs and the winner renames onto `plan.md`, so nothing matching `plan-*.md` survives
            let goal_dir = actor.goal_tracker.lock().plan_path();
            let goal_dir = goal_dir.parent().expect("plan path has a parent");
            let leaked: Vec<String> = std::fs::read_dir(goal_dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("plan-") && name.ends_with(".md"))
                .collect();
            assert!(leaked.is_empty(), "staged planner files leaked: {leaked:?}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_early_exit_clears_planning_latch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, true).await;
            create_test_goal(&actor);
            {
                let mut tracker = actor.goal_tracker.lock();
                tracker.snapshot_mut().unwrap().planning_in_flight = true;
                assert!(tracker.pause(crate::session::goal_tracker::GoalPauseReason::User));
            }

            actor.maybe_run_goal_planner("do X").await;

            assert!(
                !actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .planning_in_flight
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_success_stamps_plan_file_on_orchestration() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"# Plan\n" });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            let plan_path = actor.goal_tracker.lock().plan_path();
            let baseline_path = actor.goal_tracker.lock().plan_baseline_path();
            std::fs::create_dir_all(plan_path.parent().unwrap()).unwrap();
            std::fs::write(&plan_path, "stale plan").unwrap();
            std::fs::write(&baseline_path, "stale baseline").unwrap();

            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(snap.plan_file.as_deref(), Some(plan_path.as_path()));
            assert_eq!(
                snap.plan_baseline_file.as_deref(),
                Some(baseline_path.as_path())
            );
            assert_eq!(std::fs::read_to_string(plan_path).unwrap(), "# Plan\n");
            assert_eq!(std::fs::read_to_string(baseline_path).unwrap(), "# Plan\n");
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active,
                "success must NOT pause the goal",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_spawn_sets_harness_only_fork_context() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count, capture) =
                spawn_planner_coordinator_capturing(SpawnBehaviour::WritePlanThenDone {
                    body: b"# Plan\n",
                });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);
            let forks = capture.fork_context.lock().unwrap().clone();
            let surfaces = capture.surface_completion.lock().unwrap().clone();
            assert_eq!(forks, vec![true], "planner must request chat-prefix fork");
            assert_eq!(
                surfaces,
                vec![false],
                "planner remains harness-internal (no idle reminder)"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_fork_inherits_parent_model() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count, capture) =
                spawn_planner_coordinator_capturing(SpawnBehaviour::WritePlanThenDone {
                    body: b"# Plan\n",
                });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            // Configure an EXPLICIT planner role model different from the parent.
            // The mirror-child fork must IGNORE it and inherit the parent model, since the radix prefix is per-model
            // Without that forcing, the configured model would flow through and the assertion below would catch the regression
            let actor = StdArc::new(SessionActor {
                transient_retry_enabled: true,
                transient_retries_prompt_total: std::cell::Cell::new(0),
                transient_episode_start: std::cell::Cell::new(None),
                status_wake: Default::default(),
                goal_role_models: crate::session::GoalRoleModelConfig {
                    planner: crate::agent::config::GoalRoleModelChoice::Explicit(
                        crate::util::config::GoalRoleModel {
                            model: "some-other-planner-model".to_string(),
                            agent_type: "general-purpose".to_string(),
                        },
                    ),
                    ..Default::default()
                },
                ..StdArc::try_unwrap(actor).ok().expect("single-owner actor")
            });
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);
            let models = capture.model.lock().unwrap().clone();
            assert_eq!(
                models,
                vec![None],
                "planner fork must ignore the configured role model and inherit the parent (None)"
            );
        })
        .await;
}

/// The planner's ORIGINAL plan is snapshotted to `plan.baseline.md` once, right after the plan is written, and is NOT re-synced to later edits.
/// A second `maybe_run_goal_planner` early-returns because a plan already exists.
/// It must leave the baseline pinned to the original body even after `plan.md` itself is edited on disk.
#[tokio::test(flavor = "current_thread")]
async fn planner_snapshots_plan_baseline_once_and_does_not_overwrite() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _spawn_count) = spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone {
                body: b"# Plan v1\n",
            });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            let baseline_path = actor.goal_tracker.lock().plan_baseline_path();

            actor.maybe_run_goal_planner("do X").await;

            // Baseline recorded on the orchestration and written to disk with the planner's original body
            let recorded = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .plan_baseline_file
                .clone();
            assert_eq!(
                recorded.as_deref(),
                Some(baseline_path.as_path()),
                "plan_baseline_file must point at plan.baseline.md",
            );
            assert_eq!(
                std::fs::read_to_string(&baseline_path).unwrap(),
                "# Plan v1\n",
                "baseline must hold the planner's ORIGINAL plan body",
            );

            // The agent edits plan.md mid-run; a second planner invocation early-returns (plan already present) and must NOT re-snapshot
            let plan_path = actor.goal_tracker.lock().plan_path();
            std::fs::write(&plan_path, "# Plan v2 (agent edited)\n").unwrap();
            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(
                std::fs::read_to_string(&baseline_path).unwrap(),
                "# Plan v1\n",
                "baseline must remain the ORIGINAL plan, never overwritten",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_records_own_harness_trace_turn_with_footer() {
    // The planner subagent is represented by its OWN trace turn.
    // After `maybe_run_goal_planner`, the chat-state side buffer holds exactly one sealed harness trace turn.
    // The result keeps the `<subagent_result>` footer (with the child session id) so the trace viewer can discover the planner subagent.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"# Plan\n" });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            let turns = actor.chat_state_handle.take_harness_trace_turns().await;
            assert_eq!(turns.len(), 1, "planner rides its own trace turn");
            let [items] = turns.as_slice() else {
                panic!("planner rides its own trace turn: {turns:?}");
            };
            let [call, result] = items.as_slice() else {
                panic!("synthetic task call + result pair: {items:?}");
            };
            assert!(
                matches!(
                    call,
                    crate::sampling::ConversationItem::Assistant(a) if !a.tool_calls.is_empty()
                ),
                "first item is the synthetic task call",
            );
            let result_text = result.text_content();
            assert!(
                result_text.contains("<subagent_result>"),
                "footer present for trace-viewer / subagent discovery: {result_text}",
            );
            assert!(
                result_text.contains("subagent_id:"),
                "subagent_id present in footer: {result_text}",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_disabled_records_no_harness_trace_turn() {
    // When there is no goal or the planner is off, the fast path produces no harness trace turn
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, false).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            let turns = actor.chat_state_handle.take_harness_trace_turns().await;
            assert!(turns.is_empty(), "planner disabled — no trace turn");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_success_sets_then_clears_planning_flag() {
    // The transient "planning…" badge fires before the subagent runs (planning=Some(true))
    // The success exit path clears it with a snapshot-derived GoalUpdated (planning=None)
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"# Plan\n" });
            let (actor, _tmp, mut persistence_rx) =
                make_planner_actor_capturing(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            let flags = drain_goal_planning_flags(&mut persistence_rx);
            assert_eq!(
                flags.first(),
                Some(&Some(true)),
                "planner must emit planning=true first; got {flags:?}",
            );
            assert_eq!(
                flags.last(),
                Some(&None),
                "success path must clear the planning badge; got {flags:?}",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_clears_planning_latch_before_publishing_the_plan() {
    // Regression: the "planning…" badge must be cleared at the moment the planner run is taken and we commit to publishing the produced plan.
    // Clearing it only at the very end, after the plan/baseline I/O, is too late.
    // Before the fix the latch stayed set through the publish window, so the pager advertised "planning" while steering could no longer replan.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"# Plan\n" });
            let (actor, _tmp, mut persistence_rx) =
                make_planner_actor_capturing(Some(tx), true).await;
            create_test_goal(&actor);
            let plan_path = actor.goal_tracker.lock().plan_path();

            let observed_at_publish = StdArc::new(std::sync::Mutex::new(None::<bool>));
            let observer = {
                let actor = StdArc::clone(&actor);
                let plan_path = plan_path.clone();
                let observed = StdArc::clone(&observed_at_publish);
                tokio::task::spawn_local(async move {
                    loop {
                        if plan_path.exists() {
                            *observed.lock().unwrap() = actor
                                .goal_tracker
                                .lock()
                                .snapshot()
                                .map(|g| g.planning_in_flight);
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
            };

            actor.maybe_run_goal_planner("do X").await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), observer).await;

            assert_eq!(
                *observed_at_publish.lock().unwrap(),
                Some(false),
                "planning_in_flight must already be cleared by the time the plan is published \
                 (badge must not linger through the publish window)",
            );
            // End state: plan published, latch cleared, and the emitted flag sequence is exactly the transient badge on then off
            // There is no duplicate `planning=None` from the earlier clear plus the final catch-all
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .plan_file
                    .is_some()
            );
            let flags = drain_goal_planning_flags(&mut persistence_rx);
            assert_eq!(
                flags,
                vec![Some(true), None],
                "expected badge on then a single clear; got {flags:?}",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_fail_closed_clears_planning_flag() {
    // Even when the planner fails closed (goal paused), the last GoalUpdated must clear planning so the "planning…" badge never sticks on screen
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _spawn_count) = spawn_planner_coordinator(SpawnBehaviour::Runtime {
                message: "model rejected".into(),
                cancelled: false,
            });
            let (actor, _tmp, mut persistence_rx) =
                make_planner_actor_capturing(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            assert!(actor.goal_tracker.lock().status().unwrap().is_paused());
            let flags = drain_goal_planning_flags(&mut persistence_rx);
            assert_eq!(
                flags.first(),
                Some(&Some(true)),
                "planner must emit planning=true first; got {flags:?}",
            );
            assert_eq!(
                flags.last(),
                Some(&None),
                "fail-closed path must clear the planning badge; got {flags:?}",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planning_badge_survives_intervening_goal_update() {
    // Regression: the subagent-spawn / token-accounting `GoalUpdated` that fires while the planner runs must NOT clear the "planning…" badge
    // The latch keeps every snapshot-derived update carrying it
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp, mut persistence_rx) = make_planner_actor_capturing(None, true).await;
            create_test_goal(&actor);

            actor.emit_goal_planning(0);
            // Intervening snapshot-derived emit (mirrors the spawn path).
            let (tokens_used, finished_marginal) = actor.goal_tokens(0);
            actor.goal_notify_sender().emit_goal_updated(
                &mut actor.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );

            let flags = drain_goal_planning_flags(&mut persistence_rx);
            assert_eq!(flags.len(), 2, "expected both emits; got {flags:?}");
            assert!(
                flags.iter().all(|f| *f == Some(true)),
                "badge must persist across intervening updates; got {flags:?}",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_runtime_failure_pauses_goal_with_canonical_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) = spawn_planner_coordinator(SpawnBehaviour::Runtime {
                message: "model rejected".into(),
                cancelled: false,
            });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.plan_file.is_none(), "no plan written");
            assert!(
                snap.status.is_paused(),
                "fail-closed must pause; got {:?}",
                snap.status,
            );
            assert_eq!(
                snap.pause_message.as_deref(),
                Some(planner_failure_pause_message().as_str()),
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_runtime_cancelled_pauses_as_planner() {
    // `cancelled: true` (max turns, rewind, dequeue) is a harness failure: the wire reason stays `aborted`, the pause is the planner's, not the user's
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) = spawn_planner_coordinator(SpawnBehaviour::Runtime {
                message: "max turns".into(),
                cancelled: true,
            });
            let (actor, tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::InfraPaused
            );
            assert_eq!(
                snap.pause_message.as_deref(),
                Some(planner_failure_pause_message().as_str()),
            );
            // `InfraPaused` is shared with `Infra`; the history is what distinguishes the planner pause.
            assert!(
                snap.history.iter().any(|entry| {
                    matches!(
                        entry.event,
                        crate::session::goal_tracker::GoalEvent::PlanningFailed
                    ) && entry.detail.as_deref() == Some("aborted")
                }),
                "{:?}",
                snap.history
            );
            assert!(
                snap.history
                    .iter()
                    .any(|entry| entry.detail.as_deref() == Some("planner")),
                "{:?}",
                snap.history
            );
            let events =
                std::fs::read_to_string(tmp.path().join("events.jsonl")).expect("events.jsonl");
            assert!(
                has_event_with(&events, "goal_auto_paused", |v| v["reason"] == "planner"),
                "{events}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_missing_plan_file_pauses_goal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _spawn_count) = spawn_planner_coordinator(SpawnBehaviour::NoWriteThenDone);
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.plan_file.is_none());
            assert!(snap.status.is_paused());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_disabled_short_circuits_no_spawn_no_attempt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"x" });
            let (actor, _tmp) = make_planner_actor(Some(tx), false).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 0);
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.plan_file.is_none());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_no_coordinator_skips_silently() {
    // External harness path: planner enabled but no subagent_event_tx.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.plan_file.is_none());
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn planner_existing_plan_does_not_re_fire() {
    // Defensive: if `plan_file` is already populated (e.g. future re-trigger path that calls the helper twice), we do NOT re-spawn.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"x" });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            {
                let mut tracker = actor.goal_tracker.lock();
                tracker.snapshot_mut().unwrap().plan_file =
                    Some(std::path::PathBuf::from("/tmp/preexisting/plan.md"));
            }

            actor.maybe_run_goal_planner("do X").await;

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn reconcile_pauses_active_goal_with_no_plan() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, true).await;
            create_test_goal(&actor);
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .plan_file
                    .is_none()
            );

            actor.maybe_reconcile_active_goal_without_plan().await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.status.is_paused());
            assert_eq!(
                snap.pause_message.as_deref(),
                Some(planner_failure_pause_message().as_str()),
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn reconcile_skips_active_goal_with_plan() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, true).await;
            create_test_goal(&actor);
            {
                let mut tracker = actor.goal_tracker.lock();
                tracker.snapshot_mut().unwrap().plan_file =
                    Some(std::path::PathBuf::from("/tmp/has-plan/plan.md"));
            }

            actor.maybe_reconcile_active_goal_without_plan().await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn reconcile_is_idempotent_via_atomic_flag() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, true).await;
            create_test_goal(&actor);
            actor.maybe_reconcile_active_goal_without_plan().await;
            assert!(actor.goal_tracker.lock().status().unwrap().is_paused());

            // Re-activate the tracker directly and re-run
            // The atomic short-circuit must skip the work; a regression that removed the swap would re-pause the goal
            actor.goal_tracker.lock().resume();
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
            );
            actor.maybe_reconcile_active_goal_without_plan().await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "atomic short-circuit must prevent re-pause; reconciler ran twice",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn reconcile_skips_when_planner_disabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, false).await;
            create_test_goal(&actor);

            actor.maybe_reconcile_active_goal_without_plan().await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
            );
        })
        .await;
}

/// Planner subagent tokens fold into the goal's total via the shared `subagent_token_records` map.
/// The planner spawn routes through the same `SubagentSpawned` notification path the classifier uses.
/// The actor's notification handler tags the record with the active goal_id and `goal_tokens()` sums every matching record.
#[tokio::test(flavor = "current_thread")]
async fn planner_subagent_tokens_fold_into_goal_total() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"# Plan\n" });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            let goal_id = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .goal_id
                .clone();

            actor.maybe_run_goal_planner("do X").await;
            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);

            // Simulate the SubagentSpawned notification handler after the planner reports 12,000 cumulative tokens.
            let child_attempt = distill_message_delivery_core::AttemptId::mint(0x11).to_string();
            let mut record = SubagentTokenRecord::new(0);
            assert_eq!(
                record.spawn(Some(goal_id), Some(child_attempt.clone()), None),
                SubagentSpawnOutcome::Accepted
            );
            record.last_cumulative_reported = 12_000;
            actor
                .subagent_token_records
                .lock()
                .insert("planner-subagent-id".to_string(), record);

            // The pager adds active subagent tokens to the wire's finished-only field, so the active attempt is excluded there.
            let (tokens_used, finished_marginal) = actor.goal_tokens(0);
            assert_eq!(
                finished_marginal, 0,
                "an in-flight subagent must be excluded from `finished_marginal`",
            );
            assert!(
                tokens_used >= 12_000,
                "goal chip tokens_used must include planner marginal (got {tokens_used})",
            );
            // This mirrors the pager's live combine while the subagent runs
            // The pager sums parent_delta (0 here), finished_subagent_tokens, and its own active-subagent sum
            // That sum must never exceed the shell's total
            let active_subagent_tokens = 12_000i64;
            assert!(
                finished_marginal.saturating_add(active_subagent_tokens) <= tokens_used,
                "pager live combine must not exceed goal_tokens total while a subagent runs",
            );

            // Once the planner finishes, the attempt tokens move into completed spend.
            let mut records = actor.subagent_token_records.lock();
            let record = records
                .get_mut("planner-subagent-id")
                .expect("planner record present");
            assert_eq!(
                record.finish(
                    Some(&child_attempt),
                    SubagentFinishPayload {
                        tokens_used: 12_000,
                        ..Default::default()
                    }
                ),
                SubagentFinishOutcome::Accepted
            );
            drop(records);
            let (tokens_used, finished_marginal) = actor.goal_tokens(0);
            assert_eq!(
                finished_marginal, 12_000,
                "a sealed planner subagent marginal must reach `finished_marginal`",
            );
            assert!(tokens_used >= 12_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_fail_pause_resume_retry_success() {
    use crate::session::goal_tracker::GoalStatus;
    use std::sync::Mutex;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
            let spawn_count = StdArc::new(AtomicUsize::new(0));
            let count_task = StdArc::clone(&spawn_count);
            let plan_targets: StdArc<Mutex<Vec<String>>> = StdArc::new(Mutex::new(Vec::new()));
            let targets_task = StdArc::clone(&plan_targets);
            tokio::task::spawn_local(async move {
                while let Some(ev) = rx.recv().await {
                    if let SubagentEvent::Spawn(req) = ev {
                        let n = count_task.fetch_add(1, SeqOrd::SeqCst);
                        let plan_path = plan_path_from_prompt(&req.prompt);
                        if let Some(ref p) = plan_path {
                            targets_task.lock().unwrap().push(p.clone());
                        }
                        let result = if n == 0 {
                            SubagentResult {
                                success: false,
                                error: Some("planner failed".into()),
                                cancelled: false,
                                subagent_id: req.id.clone(),
                                child_session_id: req.id.clone(),
                                ..Default::default()
                            }
                        } else {
                            if let Some(p) = plan_path.as_deref() {
                                let _ = std::fs::create_dir_all(
                                    std::path::Path::new(p).parent().unwrap(),
                                );
                                let _ = std::fs::write(p, b"# Plan\n");
                            }
                            SubagentResult {
                                success: true,
                                output: StdArc::from("Done"),
                                subagent_id: req.id.clone(),
                                child_session_id: req.id.clone(),
                                ..Default::default()
                            }
                        };
                        let _ = req.result_tx.send(result);
                    }
                }
            });

            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;
            {
                let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
                assert!(snap.plan_file.is_none());
                assert!(snap.status.is_paused(), "got {:?}", snap.status);
                assert_eq!(
                    snap.pause_message.as_deref(),
                    Some(planner_failure_pause_message().as_str()),
                );
            }
            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);

            let _ = actor.resume_goal().await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                spawn_count.load(SeqOrd::SeqCst),
                2,
                "retry must re-fire planner"
            );
            assert_eq!(snap.status, GoalStatus::Active);
            assert!(
                snap.plan_file.is_some(),
                "successful retry writes plan_file"
            );
            let plan_path = actor.goal_tracker.lock().plan_path();
            assert_eq!(snap.plan_file.as_deref(), Some(plan_path.as_path()));
            let targets = plan_targets.lock().unwrap();
            let [first, second] = targets.as_slice() else {
                panic!("expected two plan targets: {targets:?}");
            };
            assert_ne!(first, second, "each attempt must use an isolated plan path");
            assert!(targets.iter().all(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("plan-"))
            }));
        })
        .await;
}

/// Repeated-failure variant: the planner fails, resume retries, the planner fails again, and the goal re-pauses with the canonical message.
/// This pins the retry path to the same fail-closed handling.
#[tokio::test(flavor = "current_thread")]
async fn lifecycle_fail_pause_resume_retry_fail_repauses() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Coordinator that always fails.
            let (tx, spawn_count) = spawn_planner_coordinator(SpawnBehaviour::Runtime {
                message: "still broken".into(),
                cancelled: false,
            });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);

            actor.maybe_run_goal_planner("do X").await;
            assert!(actor.goal_tracker.lock().status().unwrap().is_paused());

            let outcome = actor.resume_goal().await;
            // The planner re-failed and the goal re-paused, so resume must end the turn (Message), not flow through to inference on a paused goal
            assert!(
                matches!(outcome, GoalResumeOutcome::Message(_)),
                "re-paused resume must end the turn, not run inference",
            );

            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 2);
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.plan_file.is_none());
            assert!(snap.status.is_paused(), "got {:?}", snap.status);
            assert_eq!(
                snap.pause_message.as_deref(),
                Some(planner_failure_pause_message().as_str()),
            );
        })
        .await;
}

/// Resume of a goal that already has a `plan_file` must NOT re-fire the planner (defensive: the retry path keys off `plan_file.is_none()`).
#[tokio::test(flavor = "current_thread")]
async fn lifecycle_resume_with_plan_does_not_re_fire_planner() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"x" });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            {
                let mut tracker = actor.goal_tracker.lock();
                let snap = tracker.snapshot_mut().unwrap();
                snap.plan_file = Some(std::path::PathBuf::from("/tmp/has-plan/plan.md"));
            }
            // Pause the goal manually (planner-failure simulation).
            let _ = actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::User,
                    "user pause".into(),
                )
                .await;

            let _ = actor.resume_goal().await;

            assert_eq!(
                spawn_count.load(SeqOrd::SeqCst),
                0,
                "resume with plan present must not spawn planner",
            );
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
            );
        })
        .await;
}

#[derive(Debug, PartialEq)]
enum FakeEvent {
    CancelParentSession,
    OpenAdmission,
    Spawn { latched: bool },
}

fn spawn_latching_planner_coordinator() -> (
    tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    StdArc<std::sync::Mutex<Vec<FakeEvent>>>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    let log: StdArc<std::sync::Mutex<Vec<FakeEvent>>> =
        StdArc::new(std::sync::Mutex::new(Vec::new()));
    let log_task = StdArc::clone(&log);
    tokio::task::spawn_local(async move {
        let mut latched = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                SubagentEvent::Cancel(req)
                    if matches!(req.target, SubagentCancelTarget::ParentSession) =>
                {
                    latched = true;
                    log_task
                        .lock()
                        .unwrap()
                        .push(FakeEvent::CancelParentSession);
                }
                SubagentEvent::OpenSpawnAdmission { .. } => {
                    latched = false;
                    log_task.lock().unwrap().push(FakeEvent::OpenAdmission);
                }
                SubagentEvent::Spawn(req) => {
                    log_task.lock().unwrap().push(FakeEvent::Spawn { latched });
                    let error = if latched {
                        "parent session is stopped"
                    } else {
                        "planner failed"
                    };
                    let result = SubagentResult {
                        success: false,
                        error: Some(error.into()),
                        cancelled: latched,
                        subagent_id: req.id.clone(),
                        child_session_id: req.id.clone(),
                        ..Default::default()
                    };
                    let _ = req.result_tx.send(result);
                }
                _ => {}
            }
        }
    });
    (tx, log)
}

#[tokio::test(flavor = "current_thread")]
async fn stop_then_slash_goal_resume_reopens_spawn_admission_before_planner_retry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, log) = spawn_latching_planner_coordinator();
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            *actor.agent.borrow_mut() = test_agent_with_goal_tool().await;
            create_test_goal(&actor);
            let _ = actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::User,
                    planner_failure_pause_message(),
                )
                .await;
            {
                let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
                assert!(snap.status.is_paused(), "got {:?}", snap.status);
                assert!(snap.plan_file.is_none());
            }

            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("running"));
                state.pending_inputs.push_back(user_item("running", "test"));
            }
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            drain_gateway_turns().await;
            assert_eq!(
                *log.lock().unwrap(),
                vec![FakeEvent::CancelParentSession],
                "user Stop must latch spawn admission before the resume turn",
            );

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                actor.handle_turn_input(TurnInputRequest {
                    prompt_id: "goal-resume".into(),
                    input_origin: InputOrigin::new(PromptOrigin::User),
                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "/goal resume",
                    ))],
                    prompt_mode: PromptMode::Agent,
                    trace_gcs_config: None,
                    artifact_tracker: None,
                    client_identifier: None,
                    screen_mode: None,
                    verbatim: true,
                    send_now: false,
                    json_schema: None,
                    persist_ack: None,
                    parsed_prompt_tx: None,
                    traceparent: None,
                    start_gate: None,
                }),
            )
            .await
            .expect("turn must finish");
            assert!(
                result.is_ok(),
                "re-paused resume must end the host turn cleanly: {result:?}"
            );

            assert_eq!(
                *log.lock().unwrap(),
                vec![
                    FakeEvent::CancelParentSession,
                    FakeEvent::OpenAdmission,
                    FakeEvent::Spawn { latched: false },
                ],
                "the resume turn must reopen spawn admission before the planner retry",
            );
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.status.is_paused(), "got {:?}", snap.status);
            assert_eq!(
                snap.pause_message.as_deref(),
                Some(planner_failure_pause_message().as_str()),
            );
        })
        .await;
}

/// End-to-end gate (enabled side): when the planner is on and writes a plan, `setup_goal`'s reminder folds in the plan-aware block.
/// That block carries the actual `plan_path()` pointer plus the seed-todos, `## Deviations`, and verifier-threading instructions.
/// The legacy discipline stays intact.
#[tokio::test(flavor = "current_thread")]
async fn setup_goal_reminder_is_plan_aware_when_planner_enabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _c) = spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone {
                body: b"# Plan\n\n1. do it\n",
            });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            let plan_path = actor.goal_tracker.lock().plan_path();

            let GoalSetupOutcome::Inference { reminder } = actor.setup_goal("ship it", None).await
            else {
                panic!("a published plan must flow through to inference");
            };

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(snap.plan_file.as_deref(), Some(plan_path.as_path()));
            let expected = format!("\nPlan: {}\n", plan_path.display());
            assert!(
                reminder.contains(&expected),
                "reminder must carry the plan pointer line `{expected}`:\n{reminder}"
            );
        })
        .await;
}

/// End-to-end gate (disabled side, the default today): with the planner off, `setup_goal` writes no plan and the reminder renders the no-plan block.
/// There is no dangling `Plan:` pointer and no plan-aware phrasing, while the discipline and slim TRACKING/TEST sections remain.
#[tokio::test(flavor = "current_thread")]
async fn setup_goal_reminder_is_no_plan_when_planner_disabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, false).await;

            let GoalSetupOutcome::Inference { reminder } = actor.setup_goal("ship it", None).await
            else {
                panic!("a disabled planner must flow through to inference");
            };

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.plan_file.is_none(), "planner off writes no plan");
            assert!(
                !reminder.contains("\nPlan: "),
                "no-plan reminder must not carry a `Plan:` pointer:\n{reminder}"
            );
        })
        .await;
}

/// `/goal resume` on a planner-enabled goal with a plan must build a plan-aware reminder (carrying the real `plan_path()` pointer).
/// Guards against the resume site regressing to `None` while setup_goal stays correct (a prior regression: the sibling branch was untested).
/// The reminder is returned as the `Inference` turn content (resume flows through to inference now).
#[tokio::test(flavor = "current_thread")]
async fn goal_resume_reminder_is_plan_aware_when_planner_enabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _c) =
                spawn_planner_coordinator(SpawnBehaviour::WritePlanThenDone { body: b"x" });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            let plan_path = actor.goal_tracker.lock().plan_path();
            {
                let mut tracker = actor.goal_tracker.lock();
                tracker.snapshot_mut().unwrap().plan_file = Some(plan_path.clone());
            }
            let _ = actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::User,
                    "user pause".into(),
                )
                .await;

            let GoalResumeOutcome::Inference { reminder, .. } = actor.resume_goal().await else {
                panic!("resumed paused goal must flow through to inference");
            };

            let expected = format!("\nPlan: {}\n", plan_path.display());
            assert!(
                reminder.contains(&expected),
                "resume reminder must carry the plan pointer `{expected}`:\n{reminder}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn setup_goal_returns_message_when_planner_pauses() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) = spawn_planner_coordinator(SpawnBehaviour::Runtime {
                message: "planner crashed".into(),
                cancelled: false,
            });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;

            let GoalSetupOutcome::Message(msg) = actor.setup_goal("ship it", None).await else {
                panic!("a planner pause must end the turn, not seed inference");
            };

            assert_eq!(
                msg,
                format!("Goal paused. {}", planner_failure_pause_message())
            );
            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 1);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::InfraPaused)
            );
        })
        .await;
}

/// A `/goal pause` landing mid-plan stores no `pause_message`; the short-circuit must still produce a complete sentence.
#[tokio::test(flavor = "current_thread")]
async fn setup_goal_message_is_total_when_pause_message_missing() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            let tracker = Arc::clone(&actor.goal_tracker);
            tokio::task::spawn_local(async move {
                while let Some(ev) = rx.recv().await {
                    if let SubagentEvent::Spawn(req) = ev {
                        tracker
                            .lock()
                            .pause(crate::session::goal_tracker::GoalPauseReason::User);
                        let result = plan_written(&req, None, b"");
                        let _ = req.result_tx.send(result);
                    }
                }
            });

            let GoalSetupOutcome::Message(msg) = actor.setup_goal("ship it", None).await else {
                panic!("a goal paused mid-plan must end the turn");
            };

            assert_eq!(msg, format!("Goal paused. {GOAL_RESUME_HINT}"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resume_after_planner_failure_refires_planner_and_reports_failure_again() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) = spawn_planner_coordinator(SpawnBehaviour::Runtime {
                message: "planner crashed".into(),
                cancelled: false,
            });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            actor.maybe_run_goal_planner("do X").await;
            assert!(actor.goal_tracker.lock().status().unwrap().is_paused());

            let GoalResumeOutcome::Message(msg) = actor.resume_goal().await else {
                panic!("a re-paused resume must end the turn");
            };

            assert_eq!(
                msg,
                format!(
                    "Planning failed again; goal paused. {}",
                    planner_failure_pause_message()
                )
            );
            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 2);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::InfraPaused)
            );
        })
        .await;
}

/// A planner pause is `InfraPaused`, but a plan published on the resume supersedes it: no "prior turn failed" recap.
#[tokio::test(flavor = "current_thread")]
async fn resume_after_planner_failure_succeeds_without_infra_recap() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, spawn_count) =
                spawn_planner_coordinator(SpawnBehaviour::RuntimeThenWritePlan {
                    message: "planner crashed".into(),
                    body: b"# Plan\n",
                });
            let (actor, _tmp) = make_planner_actor(Some(tx), true).await;
            create_test_goal(&actor);
            actor.maybe_run_goal_planner("do X").await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::InfraPaused)
            );

            let GoalResumeOutcome::Inference { reminder, .. } = actor.resume_goal().await else {
                panic!("a published plan must flow through to inference");
            };

            assert!(
                !reminder.contains("Previous state: Paused (infrastructure error)."),
                "{reminder}"
            );
            assert_eq!(spawn_count.load(SeqOrd::SeqCst), 2);
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.plan_file.is_some());
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active
            );
        })
        .await;
}

/// With the planner disabled `plan_file` is always `None`, so the recap guard must key on a plan actually published, not on the retry attempt.
#[tokio::test(flavor = "current_thread")]
async fn resume_with_planner_disabled_keeps_infra_recap() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_planner_actor(None, false).await;
            create_test_goal(&actor);
            let _ = actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Infra,
                    "Turn failed: rate limit".into(),
                )
                .await;

            let GoalResumeOutcome::Inference { reminder, .. } = actor.resume_goal().await else {
                panic!("an infra resume must flow through to inference");
            };

            assert!(
                reminder.contains("Previous state: Paused (infrastructure error)."),
                "{reminder}"
            );
            assert!(
                reminder.contains("Previous error: Turn failed: rate limit"),
                "{reminder}"
            );
        })
        .await;
}
