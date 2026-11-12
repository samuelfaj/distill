// Modified for Distill by Samuel Fajreldines, 2026.
//! The per-mode policy table at run time: the policy rows a turn recorded
//! (see `crate::jev::record_policy`) and the outcome record written when a
//! root prompt turn ends.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use distill_session_events::{CheckCounts, TurnOutcomeRecord};
use distill_tool_types::{ExecutionMode, ExecutionModeChoice, TierRouting, V2Modes};

use crate::session::acp_session::SessionActor;
use crate::session::acp_session::turn_facts::{CheckOutcome, TestEvidence};
use crate::session::goal_classifier::turn_review::TurnStatus;

#[cfg(test)]
thread_local! {
    static V2_MODES_FOR_TESTS: std::cell::Cell<Option<V2Modes>> = const { std::cell::Cell::new(None) };
}

/// Overrides the configured `[modes] v2` list on this thread; `None` reads the config.
#[cfg(test)]
pub(crate) fn set_v2_modes_for_tests(modes: Option<V2Modes>) {
    V2_MODES_FOR_TESTS.with(|cell| cell.set(modes));
}

/// The modes on their v2 row, read once per process (config does not change
/// mid-run); a config that fails to load keeps the default, every mode.
pub(crate) fn v2_modes() -> V2Modes {
    #[cfg(test)]
    if let Some(modes) = V2_MODES_FOR_TESTS.with(std::cell::Cell::get) {
        return modes;
    }
    static SETTING: OnceLock<V2Modes> = OnceLock::new();
    *SETTING.get_or_init(|| {
        crate::agent::config::modes_v2(crate::config::load_effective_config().ok().as_ref())
    })
}

impl SessionActor {
    /// The policy row of the effective mode: what the harness enforces on the
    /// round being prepared. Every lever that differs by mode reads this.
    pub(super) fn mode_policy(&self) -> distill_tool_types::ModePolicy {
        v2_modes().policy(self.execution_mode.load())
    }

    /// Counts one main-loop round of the current prompt turn under `mode`.
    pub(super) fn note_mode_round(&self, mode: ExecutionMode) {
        let mut ledger = self.jev_ledger.borrow_mut();
        let rounds = ledger.mode_rounds.entry(mode.id()).or_default();
        *rounds = rounds.saturating_add(1);
    }

    /// The outcome record of the prompt turn `prompt_id`, taken before the
    /// turn's facts are drained; `None` for a subagent. The turn's per-mode
    /// counters are taken either way, so the next turn starts from zero.
    pub(super) async fn turn_outcome_record(&self, prompt_id: &str) -> Option<TurnOutcomeRecord> {
        let (mode_rounds, escalations) = {
            let mut ledger = self.jev_ledger.borrow_mut();
            (
                std::mem::take(&mut ledger.mode_rounds),
                std::mem::take(&mut ledger.turn_escalations),
            )
        };
        let policy = take_policy_rows(&self.session_id_string());
        if self.startup_hints.is_subagent {
            return None;
        }
        let choice = self.execution_mode.choice();
        let effective = self.execution_mode.load();
        // `[modes] v2` and the pinned-mode keys, resolved once for the turn.
        let root = crate::config::load_effective_config().ok();
        let (checks, review, tier_block_notified) = {
            let ledger = self.jev_ledger.borrow();
            (
                ledger.facts.recorded_checks().to_vec(),
                ledger.ultracode_turn_status(prompt_id).cloned(),
                ledger.tier_block_notified,
            )
        };
        let current_tree = if checks.is_empty() || review == Some(TurnStatus::Reviewed) {
            None
        } else {
            super::turn_facts::worktree_marker(self.tool_context.cwd.as_path()).await
        };
        let (verification, verification_reason) =
            verification_of(review.as_ref(), &checks, current_tree.as_deref());
        let effort_auto = self
            .jev_effort_auto
            .load(std::sync::atomic::Ordering::Relaxed);
        let ran: Vec<ExecutionMode> = ExecutionMode::ALL
            .into_iter()
            .filter(|mode| *mode == effective || mode_rounds.contains_key(mode.id()))
            .collect();
        let blocked = blocked_by(&Blockers {
            choice,
            ran: &ran,
            v2: crate::agent::config::modes_v2(root.as_ref()),
            no_cheaper_model: crate::agent::config::ultrasave_has_no_cheaper_model(root.as_ref()),
            jev_active: crate::jev::current_status_cached().active(),
            effort_auto,
            tier_routing_unavailable: !effort_auto
                || tier_block_notified
                || !crate::jev::lever_active(
                    distill_workspace::jev::flags::JevLever::B2ExecutionTier,
                ),
        });
        Some(TurnOutcomeRecord {
            mode_choice: choice.id().to_owned(),
            mode_rounds: mode_rounds
                .into_iter()
                .map(|(mode, rounds)| (mode.to_owned(), rounds))
                .collect(),
            verification: verification.to_owned(),
            verification_reason: verification_reason.map(str::to_owned),
            checks: check_counts(&checks),
            escalations,
            policy,
            blocked: blocked.into_iter().map(str::to_owned).collect(),
            ..Default::default()
        })
    }

    /// Completes `outcome` with the turn's subagent share once their usage
    /// has folded, then writes it: a `turn_outcome` event, the turn's
    /// usage.json row and the turn-end usage sent to the client.
    pub(super) async fn finish_turn_outcome(
        &self,
        outcome: Option<TurnOutcomeRecord>,
        usage: Option<&mut crate::extensions::notification::PromptUsage>,
    ) {
        let Some(mut outcome) = outcome else {
            return;
        };
        if let Ok(Some(ledger)) = self.chat_state_handle.try_get_prompt_usage().await {
            let children = &ledger.subagents;
            outcome.child_calls = children.model_calls;
            outcome.child_api_ms = children.api_duration_ms;
            outcome.child_cost_usd_ticks = children
                .cost_usd_ticks
                .filter(|_| !children.cost_is_partial());
        }
        self.events
            .emit(crate::session::events::Event::TurnOutcome(outcome.clone()));
        // The same prompt id the turn's `UsageTurn` named, so the row is found.
        if let Some(prompt_id) = self.current_prompt_id.lock().ok().and_then(|id| id.clone()) {
            let _ = self.notifications.persistence_tx.send(
                crate::session::persistence::PersistenceMsg::TurnOutcome {
                    prompt_id,
                    outcome: outcome.clone(),
                },
            );
        }
        if let Some(usage) = usage {
            usage.turn_outcome = Some(outcome);
        }
    }
}

/// `reviewed` when the UltraCode turn review ended REVIEWED; `checked` when
/// the latest recorded check passed on the current working tree; otherwise
/// `unverified` with why.
fn verification_of(
    review: Option<&TurnStatus>,
    checks: &[TestEvidence],
    current_tree: Option<&str>,
) -> (&'static str, Option<&'static str>) {
    if review == Some(&TurnStatus::Reviewed) {
        return ("reviewed", None);
    }
    let reason = match checks.iter().max_by_key(|check| check.finished_at) {
        None if review.is_some() => "review_unverified",
        None => "no_check",
        Some(check) => match check.outcome {
            CheckOutcome::Failed => "check_failed",
            CheckOutcome::Unknown => "check_unknown",
            CheckOutcome::Passed => match (check.worktree.as_deref(), current_tree) {
                (Some(then), Some(now)) if then == now => return ("checked", None),
                _ => "check_stale",
            },
        },
    };
    ("unverified", Some(reason))
}

fn check_counts(checks: &[TestEvidence]) -> CheckCounts {
    let mut counts = CheckCounts::default();
    for check in checks {
        let count = match check.outcome {
            CheckOutcome::Passed => &mut counts.passed,
            CheckOutcome::Failed => &mut counts.failed,
            CheckOutcome::Unknown => &mut counts.unknown,
        };
        *count = count.saturating_add(1);
    }
    counts
}

/// The state that decides what kept a mode from acting this turn.
struct Blockers<'a> {
    choice: ExecutionModeChoice,
    /// Effective modes of the turn's rounds, and the one it ended in.
    ran: &'a [ExecutionMode],
    v2: V2Modes,
    no_cheaper_model: bool,
    jev_active: bool,
    effort_auto: bool,
    tier_routing_unavailable: bool,
}

fn blocked_by(state: &Blockers<'_>) -> Vec<&'static str> {
    let mut blocked = Vec::new();
    if state.choice == ExecutionModeChoice::Ultrasave && state.no_cheaper_model {
        blocked.push("no_cheaper_model");
    }
    if !state.jev_active {
        blocked.push("jev_silent");
    }
    if !state.effort_auto {
        blocked.push("effort_pinned");
    }
    if state.choice == ExecutionModeChoice::Auto
        && state.ran.iter().any(|mode| *mode != ExecutionMode::Normal)
    {
        blocked.push("auto_runtime_only");
    }
    if state.tier_routing_unavailable
        && state
            .ran
            .iter()
            .any(|mode| state.v2.policy(*mode).runtime.tier_routing != TierRouting::Off)
    {
        blocked.push("tier_routing_off");
    }
    blocked
}

/// Policy rows each session recorded since its last turn end, by `<mode>:<rule>`.
fn session_policy_rows() -> &'static Mutex<HashMap<String, BTreeMap<String, u64>>> {
    static ROWS: OnceLock<Mutex<HashMap<String, BTreeMap<String, u64>>>> = OnceLock::new();
    ROWS.get_or_init(Default::default)
}

/// Counts one policy row for `session_id`'s current turn; nothing outside a session.
pub(crate) fn note_policy_row(session_id: &str, mode: ExecutionMode, rule: &str) {
    if session_id.is_empty() {
        return;
    }
    let Ok(mut sessions) = session_policy_rows().lock() else {
        return;
    };
    let count = sessions
        .entry(session_id.to_owned())
        .or_default()
        .entry(format!("{}:{rule}", mode.id()))
        .or_default();
    *count = count.saturating_add(1);
}

/// Drains the rows `session_id` recorded since its last turn end.
pub(crate) fn take_policy_rows(session_id: &str) -> BTreeMap<String, u64> {
    session_policy_rows()
        .lock()
        .ok()
        .and_then(|mut sessions| sessions.remove(session_id))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One rule enforcement is one counter in usage.json, one `mode_policy`
    /// event and one row for the turn outcome; a rule that is not a short
    /// identifier reaches none of them as text.
    #[tokio::test]
    #[serial_test::serial]
    async fn record_policy_writes_one_event_and_one_counter_without_free_text() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = "mode-policy-row-session";
        let writer = distill_session_events::EventWriter::open(dir.path());
        crate::jev::register_session_events(session_id, &writer);
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let recorder = distill_chat_state::ChatStateActor::spawn(
            Vec::new(),
            distill_sampling_types::SamplingConfig::default(),
            Box::new(distill_chat_state::NullChatPersistence),
            event_tx,
            tokio_util::sync::CancellationToken::new(),
        );
        let free_text = "compaction because src/secret.rs grew";
        crate::jev::with_session_scope_and_recorder(session_id, Some(recorder.clone()), async {
            crate::jev::record_policy(ExecutionMode::Ultracode, "review_verdict:reviewed");
            crate::jev::record_policy(ExecutionMode::Ultrasave, free_text);
        })
        .await;
        drop(writer);

        let events: Vec<serde_json::Value> =
            std::fs::read_to_string(dir.path().join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["type"], "mode_policy");
        assert_eq!(events[0]["mode"], "ultracode");
        assert_eq!(events[0]["rule"], "review_verdict:reviewed");
        let hashed = events[1]["rule"].as_str().unwrap();
        assert!(hashed.starts_with("h:"), "{hashed}");
        assert!(
            events
                .iter()
                .all(|event| !event.to_string().contains("secret"))
        );

        let ledger = recorder.try_get_session_usage().await.unwrap();
        let counts = |mode: &str| {
            ledger.utility_outcomes[&format!("policy:{mode}")]
                .decisions
                .clone()
        };
        assert_eq!(
            counts("ultracode"),
            BTreeMap::from([("review_verdict:reviewed".to_owned(), 1)])
        );
        assert!(
            counts("ultrasave")
                .keys()
                .all(|rule| rule.starts_with("h:"))
        );

        let rows = take_policy_rows(session_id);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows["ultracode:review_verdict:reviewed"], 1);
        assert!(rows.keys().all(|key| !key.contains("secret")));
        assert!(
            take_policy_rows(session_id).is_empty(),
            "a turn end drains them"
        );
    }

    fn bash(
        command: &str,
        exit_code: i32,
        output: &str,
    ) -> distill_tools::types::output::ToolOutput {
        distill_tools::types::output::ToolOutput::Bash(distill_tools::types::output::BashOutput {
            output: Vec::new(),
            output_for_prompt: output.to_owned(),
            exit_code,
            command: command.to_owned(),
            truncated: false,
            signal: None,
            timed_out: false,
            description: None,
            current_dir: "/repo".to_owned(),
            output_file: String::new(),
            total_bytes: 0,
            output_delta: None,
            was_bare_echo: false,
        })
    }

    /// Records checks in order, a moment apart so their finish times differ.
    fn checks(runs: &[(&str, i32, &str, Option<&str>)]) -> Vec<TestEvidence> {
        let mut facts = crate::session::acp_session::turn_facts::TurnFacts::default();
        for (index, (command, exit, output, tree)) in runs.iter().enumerate() {
            std::thread::sleep(std::time::Duration::from_millis(2));
            facts.note_tool_result_at(
                &index.to_string(),
                &bash(command, *exit, output),
                &crate::session::acp_session::turn_facts::CheckContext {
                    cwd: "/repo".to_owned(),
                    worktree: tree.map(str::to_owned),
                },
            );
        }
        facts.recorded_checks().to_vec()
    }

    /// A turn is `checked` only by a pass of its latest check on the tree as it
    /// is now, and `reviewed` only by a passing UltraCode review; everything
    /// else is `unverified` with a reason a dashboard can group by.
    #[test]
    fn verification_names_the_strongest_evidence_the_turn_has() {
        let pass = ("cargo test", 0, "test result: ok", Some("t1"));
        let fail = (
            "cargo test",
            101,
            "test result: FAILED. 1 failed",
            Some("t1"),
        );
        let unknown = ("cargo build 2>&1 | grep -E '^error'", 1, "", Some("t1"));
        let reviewed = TurnStatus::Reviewed;
        let refuted = TurnStatus::Unverified("still refuted".into());
        let cases: [(Option<&TurnStatus>, Vec<TestEvidence>, Option<&str>, _); 9] = [
            (Some(&reviewed), Vec::new(), None, ("reviewed", None)),
            (None, checks(&[pass]), Some("t1"), ("checked", None)),
            (None, checks(&[fail, pass]), Some("t1"), ("checked", None)),
            (None, Vec::new(), None, ("unverified", Some("no_check"))),
            (
                None,
                checks(&[pass, fail]),
                Some("t1"),
                ("unverified", Some("check_failed")),
            ),
            (
                None,
                checks(&[unknown]),
                Some("t1"),
                ("unverified", Some("check_unknown")),
            ),
            (
                None,
                checks(&[pass]),
                Some("t2"),
                ("unverified", Some("check_stale")),
            ),
            (
                None,
                checks(&[pass]),
                None,
                ("unverified", Some("check_stale")),
            ),
            (
                Some(&refuted),
                Vec::new(),
                None,
                ("unverified", Some("review_unverified")),
            ),
        ];
        for (index, (review, recorded, tree, expected)) in cases.into_iter().enumerate() {
            assert_eq!(
                verification_of(review, &recorded, tree),
                expected,
                "case {index}"
            );
        }
        // An identical rerun with the same outcome is one recorded check.
        assert_eq!(
            check_counts(&checks(&[pass, fail, unknown, pass])),
            CheckCounts {
                passed: 1,
                failed: 1,
                unknown: 1
            }
        );
    }

    /// Each blocker names state that already exists and only for a mode that
    /// would have acted: tier routing matters only where a row routes tiers,
    /// and Auto limits only a turn that ran a non-Normal mode.
    #[test]
    fn blocked_lists_what_kept_the_mode_from_acting() {
        let open = |choice, ran: &'static [ExecutionMode]| Blockers {
            choice,
            ran,
            v2: V2Modes::default(),
            no_cheaper_model: false,
            jev_active: true,
            effort_auto: true,
            tier_routing_unavailable: false,
        };
        assert!(
            blocked_by(&open(ExecutionModeChoice::Normal, &[ExecutionMode::Normal])).is_empty()
        );
        assert!(blocked_by(&open(ExecutionModeChoice::Auto, &[ExecutionMode::Normal])).is_empty());
        assert_eq!(
            blocked_by(&open(
                ExecutionModeChoice::Auto,
                &[ExecutionMode::Normal, ExecutionMode::Ultracode]
            )),
            ["auto_runtime_only"]
        );
        let all = Blockers {
            no_cheaper_model: true,
            jev_active: false,
            effort_auto: false,
            tier_routing_unavailable: true,
            ..open(ExecutionModeChoice::Ultrasave, &[ExecutionMode::Ultrasave])
        };
        assert_eq!(
            blocked_by(&all),
            [
                "no_cheaper_model",
                "jev_silent",
                "effort_pinned",
                "tier_routing_off"
            ]
        );
        let no_routing_mode = Blockers {
            tier_routing_unavailable: true,
            ..open(ExecutionModeChoice::Ultracode, &[ExecutionMode::Ultracode])
        };
        assert!(blocked_by(&no_routing_mode).is_empty());
        let auto_save = Blockers {
            no_cheaper_model: true,
            tier_routing_unavailable: true,
            ..open(ExecutionModeChoice::Auto, &[ExecutionMode::Ultrasave])
        };
        assert_eq!(
            blocked_by(&auto_save),
            ["auto_runtime_only", "tier_routing_off"],
            "the cheaper model is a pinned-session lever"
        );
    }

    /// A subagent turn writes no record, but its counters still reset so a
    /// later turn does not inherit them.
    #[tokio::test(flavor = "current_thread")]
    async fn a_subagent_turn_writes_no_record() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut actor = super::super::support::plain_actor().await;
                actor.startup_hints.is_subagent = true;
                actor.note_mode_round(ExecutionMode::Ultrafast);
                actor.jev_ledger.borrow_mut().turn_escalations = 2;
                assert_eq!(actor.turn_outcome_record("p1").await, None);
                assert!(actor.jev_ledger.borrow().mode_rounds.is_empty());
                assert_eq!(actor.jev_ledger.borrow().turn_escalations, 0);
            })
            .await;
    }

    /// A real root turn against a scripted model (a tool round, then the
    /// answer) writes one record: its rounds by mode, its verification, and
    /// the same record on the turn-end usage and in the usage.json message.
    #[test]
    fn a_root_turn_writes_one_outcome_record() {
        use distill_test_support::sse::{
            responses_api_reasoning_then_tool_call_events, responses_api_script_exact,
        };
        use distill_test_support::{MockInferenceServer, ScriptedResponse};
        const TODO_ARGS: &str = r#"{"todos":[{"id":"t1","content":"poll","status":"completed"}]}"#;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            // A debug-build turn future outgrows the usual 16 MiB test stack.
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                tokio::task::LocalSet::new().block_on(&runtime, async move {
                    let server = MockInferenceServer::start().await.expect("mock server");
                    server.enqueue_response(
                        "/v1/responses",
                        ScriptedResponse::sse(responses_api_reasoning_then_tool_call_events(
                            "update",
                            "call-1",
                            "todo_write",
                            TODO_ARGS,
                            "test",
                        )),
                    );
                    server.enqueue_response(
                        "/v1/responses",
                        ScriptedResponse::sse(responses_api_script_exact("Done.", "test")),
                    );
                    let (actor, persistence) = root_actor_for(&server).await;
                    actor.execution_mode.store(ExecutionMode::Ultrasave);
                    *actor.current_prompt_id.lock().unwrap() = Some("outcome-turn".into());
                    // Every Jev question in the turn goes unanswered, without a call.
                    crate::jev::set_test_decision_answers(std::iter::repeat_n(None, 64));
                    let outcome = tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        actor.handle_prompt(
                            "outcome-turn",
                            vec![agent_client_protocol::ContentBlock::Text(
                                agent_client_protocol::TextContent::new("mark the todo done"),
                            )],
                            crate::session::plan_mode::PromptMode::Agent,
                            None,
                            None,
                            None,
                            None,
                            true,
                            false,
                            None,
                            None,
                            None,
                        ),
                    )
                    .await
                    .expect("turn must finish within timeout");
                    crate::jev::clear_test_decision_answers();
                    let usage = outcome.expect("turn must not error").usage;
                    tokio::task::yield_now().await;
                    let stored = persistence.borrow().clone();
                    sender
                        .send((usage.and_then(|usage| usage.turn_outcome), stored))
                        .expect("send result");
                });
            })
            .expect("spawn test thread")
            .join()
            .expect("test thread");
        let (sent, stored) = receiver.recv().expect("turn result");
        let sent = sent.expect("the turn-end usage carries the record");
        assert_eq!(
            stored,
            [("outcome-turn".to_owned(), sent.clone())],
            "one record per turn"
        );
        assert_eq!(sent.mode_choice, "ultrasave");
        assert_eq!(
            sent.mode_rounds,
            BTreeMap::from([("ultrasave".to_owned(), 2)])
        );
        assert_eq!(sent.verification, "unverified");
        assert_eq!(sent.verification_reason.as_deref(), Some("no_check"));
        assert_eq!(sent.checks, CheckCounts::default());
        assert_eq!(sent.escalations, 0);
        assert_eq!(sent.child_calls, 0);
        assert!(
            sent.blocked
                .iter()
                .any(|reason| reason == "no_cheaper_model"),
            "{:?}",
            sent.blocked
        );
        assert!(
            !sent
                .blocked
                .iter()
                .any(|reason| reason == "auto_runtime_only")
        );
    }

    type StoredOutcomes = std::rc::Rc<std::cell::RefCell<Vec<(String, TurnOutcomeRecord)>>>;

    /// A root actor whose model is `server`, with the turn outcomes it sent to
    /// persistence.
    async fn root_actor_for(
        server: &distill_test_support::MockInferenceServer,
    ) -> (std::sync::Arc<SessionActor>, StoredOutcomes) {
        use super::super::support::*;
        use crate::session::persistence::PersistenceMsg;
        let sampler_config = distill_sampler::SamplerConfig {
            api_key: Some("test-key".to_string()),
            base_url: server.url(),
            model: "test".to_string(),
            api_backend: distill_sampler::ApiBackend::Responses,
            context_window: 256_000,
            max_retries: Some(0),
            idle_timeout_secs: Some(30),
            ..Default::default()
        };
        let (sampler_event_tx, mut sampler_event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sampler_handle = distill_sampler::SamplerActor::spawn(
            sampler_config,
            distill_sampler::RetryPolicy {
                max_retries: 0,
                ..Default::default()
            },
            sampler_event_tx,
        );
        let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
        drain_gateway(gateway_rx);
        let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
        let stored = StoredOutcomes::default();
        let sink = stored.clone();
        tokio::task::spawn_local(async move {
            while let Some(message) = persistence_rx.recv().await {
                match message {
                    PersistenceMsg::FlushAndAck { respond_to }
                    | PersistenceMsg::CompactionCheckpointAndAck { respond_to, .. } => {
                        let _ = respond_to.send(Ok(()));
                    }
                    PersistenceMsg::TurnOutcome { prompt_id, outcome } => {
                        sink.borrow_mut().push((prompt_id, outcome));
                    }
                    _ => {}
                }
            }
        });
        let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        actor.sampler_handle = sampler_handle;
        *actor.agent.borrow_mut() = test_distill_agent_with_todo().await;
        let mut cfg = actor
            .chat_state_handle
            .get_sampling_config()
            .await
            .expect("test actor has sampling config");
        cfg.base_url = server.url();
        cfg.api_backend = distill_sampling_types::ApiBackend::Responses;
        cfg.model = "test".to_string();
        actor.chat_state_handle.update_sampling_config(cfg);
        let mut creds = actor.chat_state_handle.get_credentials().await;
        creds.api_key = Some("test-key".to_string());
        actor.chat_state_handle.update_credentials(creds);
        actor
            .workspace_ops
            .bind_local_session(
                &actor.session_id_string(),
                actor.tool_context.cwd.as_path().to_path_buf(),
                actor.tool_context.hunk_tracker_handle.clone(),
                actor.agent.borrow().tool_bridge().toolset(),
                None,
            )
            .expect("bind_local_session");
        let actor = std::sync::Arc::new(actor);
        let drainer = actor.clone();
        tokio::task::spawn_local(async move {
            while let Some(event) = sampler_event_rx.recv().await {
                drainer.handle_sampling_event(event).await;
            }
        });
        (actor, stored)
    }
}
