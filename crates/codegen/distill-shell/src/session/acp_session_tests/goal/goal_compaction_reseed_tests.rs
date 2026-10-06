// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;
use crate::session::helpers::compaction_context::{CompactionInputs, CompactionStateContext};
use distill_chat_state::compaction_utils::{
    CompactedHistoryInput, build_compacted_history, wrap_user_query,
};
use distill_sampling_types::{ConversationItem, SyntheticReason};
use std::sync::Arc as StdArc;
use tempfile::TempDir;

const DEVICE_TEST_OBJECTIVE: &str = "ssh to root@example.test and test that a genbw meter profile does NOT produce aggregate meter data on dnp3 or modbus";
const STALE_CODE_REVIEW: &str = "please review the PR for config-json-go";

#[tokio::test(flavor = "current_thread")]
async fn goal_recheck_retires_resolved_gaps_without_completing_pending_work() {
    use crate::session::goal_classifier::parse_verdict_path_from_prompt;
    use crate::session::goal_tracker::GoalStatus;
    use distill_tools::implementations::distill::task::types::{SubagentEvent, SubagentResult};

    tokio::task::LocalSet::new().run_until(async {
        let tmp = TempDir::new().unwrap();
        let delivery = tmp.path().join("delivery");
        std::fs::create_dir(&delivery).unwrap();
        std::fs::write(delivery.join("cors.rs"), "before\n").unwrap();
        for args in [
            vec!["init", "-q"], vec!["add", "."],
            vec!["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", "commit", "-qm", "baseline"],
        ] {
            assert!(std::process::Command::new(crate::util::subprocess::git_bin())
                .args(args).current_dir(&delivery).output().unwrap().status.success());
        }
        let baseline = crate::session::goal_classifier::capture_git_baseline(&delivery).await.unwrap();
        std::fs::write(delivery.join("cors.rs"), "after\n").unwrap();
        let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
        super::rate_limit_backoff_tests::drain_gateway(gateway_rx);
        let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
        super::rate_limit_backoff_tests::drain_persistence(persistence_rx);
        let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        actor.goal_enabled = true;
        actor.goal_classifier_enabled = true;
        actor.goal_use_current_model_only = true;
        actor.goal_tracker = StdArc::new(parking_lot::Mutex::new(
            crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf()),
        ));
        set_goal_harness_for_tests(&actor);
        start_device_test_goal(&actor);
        {
            let mut tracker = actor.goal_tracker.lock();
            let goal = tracker.snapshot_mut().unwrap();
            goal.last_classifier_gaps = Some("The delivery diff captured the wrong repository".into());
            goal.progress.no_progress_rounds = 3;
            goal.progress.verification_target = Some(crate::session::goal_evaluator::GoalVerificationTarget {
                workspace_root: delivery.to_string_lossy().into_owned(), baseline_commit: baseline,
            });
            goal.progress.criteria.push(serde_json::from_value(serde_json::json!({
                "id":"runtime", "requirement":"verify deployed behavior", "source":"user requirement",
                "status":"pending", "evidence":"", "scope":"development", "invalidated_by":""
            })).unwrap());
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        actor.tool_context.subagent_event_tx = Some(tx);
        let coordinator = tokio::task::spawn_local(async move {
            while let Some(event) = rx.recv().await {
                if let SubagentEvent::Spawn(req) = event {
                    assert!(req.prompt.contains("Independently recheck ONLY the prior findings"));
                    assert_eq!(req.cwd.as_deref(), delivery.to_str());
                    assert!(req.prompt.contains("- cors.rs"));
                    let path = parse_verdict_path_from_prompt(&req.prompt).unwrap();
                    tokio::fs::write(path, r#"{"refuted":false,"evidence":"Git diff matches the delivery worktree","confidence":"high"}"#).await.unwrap();
                    let result = SubagentResult {
                        success: true, output: StdArc::from("Not Refuted"),
                        subagent_id: req.id.clone(), child_session_id: req.id.clone(),
                        ..Default::default()
                    };
                    req.result_tx.send(result).unwrap();
                    break;
                }
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), actor.verify_goal_candidate(true)).await.unwrap();
        coordinator.await.unwrap();
        let tracker = actor.goal_tracker.lock();
        let goal = tracker.snapshot().unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
        assert!(goal.last_classifier_gaps.is_none());
        assert_eq!(goal.progress.no_progress_rounds, 0);
        assert_eq!(goal.progress.criteria[0].status, crate::session::goal_evaluator::GoalCriterionStatus::Pending);
        assert_eq!(goal.classifier_runs_attempted, 1);
        drop(tracker);
        actor.goal_tracker.lock().clear();
    }).await;
}

/// Serves the session model `test` from `server`. Jev decisions are answered
/// locally so the main model's effort passes stay offline.
fn install_main_model(actor: &SessionActor, server: &distill_test_support::MockInferenceServer) {
    let mut entry = crate::agent::config::ModelEntry::fallback(
        "test",
        &crate::agent::config::EndpointsConfig::default(),
    );
    entry.info.base_url = server.url();
    entry.info.api_backend = distill_sampling_types::ApiBackend::Responses;
    entry.api_key = Some("test-key".to_owned());
    actor.models_manager.insert_test_entry("test", entry);
    actor
        .models_manager
        .set_current_model_id(agent_client_protocol::ModelId::new("test"));
    crate::jev::set_test_decision_answers([]);
}

/// The model the session's next round is sent to.
async fn next_round_model(actor: &SessionActor) -> String {
    crate::jev::with_session_scope_and_recorder("goal-stall-test", None, async {
        actor.prepare_sampler_for_turn().await.model
    })
    .await
}

/// A goal that stops making progress is never paused for it. From the second
/// evaluation without progress the main model is told to change approach and
/// keeps the goal's rounds, with no side executor, and the persisted proof
/// survives compaction throughout.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn a_stalled_goal_changes_approach_without_pausing() {
    use super::rate_limit_backoff_tests::actor_under_test_with_subagents;
    use crate::session::goal_tracker::{GoalEvent, GoalStatus};
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};
    use distill_tools::implementations::distill::task::types::SubagentEvent;

    tokio::task::LocalSet::new().run_until(async {
        let server = MockInferenceServer::start().await.unwrap();
        let verdict = serde_json::json!({
            "decision": "continue", "evidence": "unchanged report requirement",
            "next_step": "resolve the required skill", "blocker_key": "",
            "progress_evidence": "", "needs_review_panel": false,
            "observations": [], "verification_target": null,
            "criteria": [{"id":"ui", "requirement":"post-deploy test",
                "source":"user: test after deploy", "status":"verified",
                "evidence":"playwright.log:12 PASS", "scope":"abc / development", "invalidated_by":""}]
        });
        for _ in 0..4 {
            server.enqueue_response("/v1/responses", ScriptedResponse::sse(
                responses_api_script_exact(&verdict.to_string(), "test"),
            ));
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = spawns.clone();
        let coordinator = tokio::task::spawn_local(async move {
            while let Some(event) = rx.recv().await {
                if matches!(event, SubagentEvent::Spawn(_)) {
                    seen.set(seen.get() + 1);
                }
            }
        });
        let actor = actor_under_test_with_subagents(&server, tx).await;
        install_main_model(&actor, &server);
        set_goal_harness_for_tests(&actor);
        start_device_test_goal(&actor);
        for round in 0..=3 {
            if round == 1 {
                actor.chat_state_handle.replace_conversation_for_compaction(vec![
                    ConversationItem::system("Compacted conversation"),
                ]);
                actor.reseed_active_goal_after_compaction().await;
                assert!(actor.chat_state_handle.get_conversation().await.iter()
                    .any(|item| item.text_content().contains("playwright.log:12 PASS")));
            }
            let checkpoint = round == 1 || round == 3;
            let worker_rounds = actor.goal_tracker.lock().snapshot().unwrap().total_worker_rounds;
            let decision = if checkpoint {
                actor.run_goal_progress_checkpoint().await
            } else { actor.run_goal_round_end().await };
            assert!(matches!(decision, GoalRoundDecision::Continue(_)), "round {round} must not pause");
            let tracker = actor.goal_tracker.lock();
            let goal = tracker.snapshot().unwrap();
            assert_eq!(goal.status, GoalStatus::Active);
            assert_eq!(goal.progress.no_progress_rounds, round);
            assert_eq!(goal.progress.criteria[0].evidence, "playwright.log:12 PASS");
            assert_eq!(goal.escalation_runs, round.saturating_sub(1));
            if checkpoint {
                assert_eq!(goal.total_worker_rounds, worker_rounds, "a checkpoint does not finish a worker round");
                assert_eq!(goal.classifier_runs_attempted, 0, "checkpoints never trigger a verifier");
            }
        }
        assert_eq!(spawns.get(), 0, "no side executor: the main model keeps the goal");
        assert_eq!(next_round_model(&actor).await, "test", "the goal's next round stays on the main model");
        assert!(actor.chat_state_handle.get_conversation().await.iter()
            .any(|item| item.text_content().contains("goal evaluations in a row found no verifiable progress")));
        {
            let tracker = actor.goal_tracker.lock();
            let goal = tracker.snapshot().unwrap();
            assert!(goal.history.iter().any(|entry| matches!(entry.event, GoalEvent::Escalated)
                && entry.detail.as_deref().is_some_and(|detail| detail.contains("without progress"))));
        }
        coordinator.abort();
        crate::jev::clear_test_decision_answers();
    }).await;
}

/// Progress after a stall ends it: the escalation count resets and the history
/// says the stall resolved.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn observed_progress_resolves_the_escalation() {
    use super::rate_limit_backoff_tests::{SessionKind, actor_under_test, sampler_surfaces_429};
    use crate::session::goal_tracker::GoalEvent;
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let verdict = |observations: serde_json::Value| {
                serde_json::json!({
                    "decision": "continue", "evidence": "registration not proven yet",
                    "next_step": "drive the registration spec", "blocker_key": "",
                    "progress_evidence": "", "needs_review_panel": false,
                    "observations": observations, "verification_target": null,
                    "criteria": [{"id":"register", "requirement":"register a new user",
                        "source":"user: prove registration", "status":"pending",
                        "evidence":"", "scope":"", "invalidated_by":""}]
                })
            };
            let passed = serde_json::json!([{"criterion_id":"register", "artifact":"e2e/register",
            "revision":"abc", "outcome":"passed"}]);
            for response in [
                verdict(serde_json::json!([])),
                verdict(serde_json::json!([])),
                verdict(serde_json::json!([])),
                verdict(passed),
            ] {
                server.enqueue_response(
                    "/v1/responses",
                    ScriptedResponse::sse(responses_api_script_exact(
                        &response.to_string(),
                        "test",
                    )),
                );
            }
            let (actor, _) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
            install_main_model(&actor, &server);
            set_goal_harness_for_tests(&actor);
            start_device_test_goal(&actor);
            for _ in 0..3 {
                actor.run_goal_round_end().await;
            }
            assert_eq!(actor.goal_tracker.lock().snapshot().unwrap().escalation_runs, 1);
            actor.run_goal_round_end().await;
            {
                let tracker = actor.goal_tracker.lock();
                let goal = tracker.snapshot().unwrap();
                assert_eq!(goal.progress.no_progress_rounds, 0);
                assert_eq!(goal.escalation_runs, 0);
                assert!(
                    goal.history
                        .iter()
                        .any(|entry| matches!(entry.event, GoalEvent::EscalationResolved))
                );
            }
            assert_eq!(next_round_model(&actor).await, "test");
            crate::jev::clear_test_decision_answers();
        })
        .await;
}

/// The verifier rejecting completion with the same gaps again used to pause the
/// goal on the second rejection. It now restructures and escalates the gaps;
/// only the configured rejection cap still pauses.
#[tokio::test(flavor = "current_thread")]
async fn repeated_verifier_gaps_escalate_instead_of_pausing() {
    use crate::session::goal_classifier::GoalClassifierOutcome;
    use crate::session::goal_tracker::{GoalEvent, GoalStatus};

    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            let policy = super::goal_support::GoalClassifierPolicy {
                enabled: true,
                max_runs: 10,
            };
            let notify = actor.goal_notify_sender();
            for attempt in 1..=4 {
                actor
                    .apply_classifier_outcome(
                        &policy,
                        attempt,
                        GoalClassifierOutcome::NotAchieved {
                            details_path: String::new(),
                            gaps_summary: "meter data still aggregated on dnp3".into(),
                            pause_summary: "meter data still aggregated on dnp3".into(),
                            gap_fingerprint: "dnp3-aggregate".into(),
                        },
                        &notify,
                    )
                    .await;
                assert_eq!(
                    actor.goal_tracker.lock().status(),
                    Some(GoalStatus::Active),
                    "attempt {attempt}"
                );
            }
            let tracker = actor.goal_tracker.lock();
            let goal = tracker.snapshot().unwrap();
            assert!(
                goal.escalation_runs >= 1,
                "the repeated gaps were escalated"
            );
            assert!(
                goal.history
                    .iter()
                    .any(|entry| matches!(entry.event, GoalEvent::Escalated))
            );
        })
        .await;
}

/// A human gate is confirmed at the first checkpoint that sees it: the goal
/// pauses there with the user's options instead of working 25 more minutes and
/// three rounds to re-confirm it. A transient blocker keeps working.
#[tokio::test(flavor = "current_thread")]
async fn a_blocker_only_the_user_can_clear_pauses_at_the_first_checkpoint() {
    use super::rate_limit_backoff_tests::{SessionKind, actor_under_test, sampler_surfaces_429};
    use crate::session::goal_tracker::GoalStatus;
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    tokio::task::LocalSet::new().run_until(async {
        let server = MockInferenceServer::start().await.unwrap();
        let blocked = |kind: &str| serde_json::json!({
            "decision": "blocked", "evidence": "DEV-4662 is labeled HUMAN GATE and DEV-4658 is In Progress",
            "next_step": "Authorize DEV-4662 or finish DEV-4658. Options: 1) take the next eligible Todo DEV-3275",
            "blocker_key": "dev_4662_human_gate", "blocker_kind": kind,
            "progress_evidence": "", "needs_review_panel": false,
            "observations": [], "verification_target": null, "criteria": []
        });
        for kind in ["transient", "requires_user"] {
            server.enqueue_response("/v1/responses", ScriptedResponse::sse(
                responses_api_script_exact(&blocked(kind).to_string(), "test"),
            ));
        }
        let (actor, _) = actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
        set_goal_harness_for_tests(&actor);
        start_device_test_goal(&actor);
        assert!(matches!(actor.run_goal_progress_checkpoint().await, GoalRoundDecision::Continue(_)));
        assert_eq!(actor.goal_tracker.lock().status(), Some(GoalStatus::Active), "a transient blocker keeps working");
        assert!(matches!(actor.run_goal_progress_checkpoint().await, GoalRoundDecision::EndTurn));
        let tracker = actor.goal_tracker.lock();
        let goal = tracker.snapshot().unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
        let message = goal.pause_message.as_deref().unwrap();
        assert!(message.contains("DEV-3275"), "the pause names the user's options: {message}");
        assert_eq!(goal.total_worker_rounds, 0, "paused mid-round, before re-confirming the gate");
    }).await;
}

/// `/goal resume <text>` hands the user's decision to the resumed goal.
#[tokio::test(flavor = "current_thread")]
async fn resume_guidance_reaches_the_resumed_turn() {
    use crate::session::goal_tracker::GoalPauseReason;
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .pause(GoalPauseReason::Verification)
            );
            let GoalResumeOutcome::Inference { reminder, .. } = actor
                .resume_goal(Some("  take the next Todo: DEV-3275 "))
                .await
            else {
                panic!("a paused goal resumes into a turn");
            };
            assert!(reminder.contains(
                "<goal_resume_guidance>\ntake the next Todo: DEV-3275\n</goal_resume_guidance>"
            ));
            assert!(reminder.contains("A goal has been set:"));
        })
        .await;
}

/// Every evaluation leaves a record of why the goal continued, escalated or
/// paused. The verdicts that paused the three failing sessions were lost.
#[tokio::test(flavor = "current_thread")]
async fn each_goal_evaluation_and_escalation_is_recorded() {
    use super::rate_limit_backoff_tests::{SessionKind, actor_under_test, sampler_surfaces_429};
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let verdict = serde_json::json!({
                "decision": "continue", "evidence": "registration not proven yet",
                "next_step": "drive the registration spec", "blocker_key": "", "blocker_kind": "",
                "progress_evidence": "", "needs_review_panel": false,
                "observations": [], "verification_target": null,
                "criteria": [{"id":"register", "requirement":"register a new user",
                    "source":"user: prove registration", "status":"pending",
                    "evidence":"", "scope":"", "invalidated_by":""}]
            });
            for _ in 0..3 {
                server.enqueue_response(
                    "/v1/responses",
                    ScriptedResponse::sse(responses_api_script_exact(&verdict.to_string(), "test")),
                );
            }
            let (actor, _) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
            let tmp = TempDir::new().unwrap();
            *actor.goal_tracker.lock() =
                crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf());
            set_goal_harness_for_tests(&actor);
            start_device_test_goal(&actor);
            actor.run_goal_progress_checkpoint().await;
            actor.run_goal_round_end().await;
            actor.run_goal_round_end().await;
            let log = std::fs::read_to_string(tmp.path().join("goal/evaluations.jsonl")).unwrap();
            let records: Vec<serde_json::Value> = log
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let summary: Vec<(&str, &str)> = records
                .iter()
                .map(|record| {
                    (
                        record["kind"].as_str().unwrap(),
                        record["action"].as_str().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                summary,
                [
                    ("checkpoint", "continue"),
                    ("round_end", "continue"),
                    ("round_end", "escalate"),
                    ("escalation", "change_approach"),
                ]
            );
            assert_eq!(records[2]["no_progress_rounds"], 2);
            assert_eq!(records[2]["next_step"], "drive the registration spec");
            assert!(
                records
                    .iter()
                    .all(|record| record["goal_id"] == "g-device-test" && record["ts"].is_string())
            );
        })
        .await;
}

async fn make_goal_actor() -> (StdArc<SessionActor>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_tracker = StdArc::new(parking_lot::Mutex::new(
        crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf()),
    ));
    (StdArc::new(actor), tmp)
}

fn start_device_test_goal(actor: &SessionActor) {
    actor.goal_tracker.lock().create_goal(
        "g-device-test".into(),
        DEVICE_TEST_OBJECTIVE.into(),
        None,
        0,
        "2026-08-06T00:00:00Z".into(),
        None,
    );
}

fn stale_pre_goal_conversation() -> Vec<ConversationItem> {
    vec![
        ConversationItem::system("You are Grok."),
        ConversationItem::user(format!(
            "<user_info>OS: linux</user_info>\n\n<user_query>\n{STALE_CODE_REVIEW}\n</user_query>"
        )),
        ConversationItem::assistant("I'll start the branch code review."),
        ConversationItem::system_reminder(format!(
            "A goal has been set: {DEVICE_TEST_OBJECTIVE}\nStart now."
        )),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn last_user_query_seeds_from_active_goal_not_stale_pre_goal_prompt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            let conversation = stale_pre_goal_conversation();

            let without_goal =
                CompactionStateContext::build(&conversation, CompactionInputs::default()).await;
            assert_eq!(
                without_goal.last_user_query.as_deref(),
                Some(STALE_CODE_REVIEW),
                "without the goal field, compact would revive the pre-goal code review"
            );

            let with_goal = CompactionStateContext::build(
                &conversation,
                CompactionInputs {
                    goal_objective: actor.goal_objective_for_compaction(),
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(
                with_goal.last_user_query.as_deref(),
                Some(DEVICE_TEST_OBJECTIVE),
                "active goal must seed last_user_query from GoalTracker.objective"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_goal_section_is_continuation_not_create_style_rules() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);

            let section = actor
                .compaction_goal_section()
                .await
                .expect("active goal must produce a compaction reminder section");
            assert!(
                section.contains(DEVICE_TEST_OBJECTIVE),
                "reminder must pin the live objective:\n{section}"
            );
            assert!(
                section.contains(GOAL_CONTINUATION_SENTINEL),
                "reminder must be continuation-style, not create-style rules:\n{section}"
            );
            assert!(
                !section.contains("A goal has been set"),
                "create-style goal_rules must not be the post-compact cue:\n{section}"
            );
            assert!(
                section.contains("Do not restart this goal"),
                "reminder must tell the model not to restart:\n{section}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn summarizer_user_context_pins_objective() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);

            let ctx = actor
                .merge_goal_compaction_user_context(None)
                .expect("active goal must produce summarizer context");
            assert!(ctx.contains(DEVICE_TEST_OBJECTIVE), "{ctx}");
            assert!(ctx.contains("Do not restart this goal"), "{ctx}");

            let merged = actor
                .merge_goal_compaction_user_context(Some("keep auth".into()))
                .expect("merge keeps caller text");
            assert!(merged.contains("keep auth"), "{merged}");
            assert!(merged.contains(DEVICE_TEST_OBJECTIVE), "{merged}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn post_compact_history_keeps_objective_and_goal_summary_continuation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            let conversation = stale_pre_goal_conversation();
            actor
                .chat_state_handle
                .replace_conversation(conversation.clone());
            let _ = actor.chat_state_handle.get_conversation().await;

            let state_context = CompactionStateContext::build(
                &conversation,
                CompactionInputs {
                    goal_objective: actor.goal_objective_for_compaction(),
                    ..Default::default()
                },
            )
            .await
            .for_compaction();
            assert_eq!(
                state_context.last_user_query.as_deref(),
                Some(DEVICE_TEST_OBJECTIVE)
            );

            let goal_section = actor
                .compaction_goal_section()
                .await
                .expect("active goal reminder");
            let system_reminder = format!(
                "<system-reminder>\n## Files Edited This Session\n- src/auth.rs\n\n{goal_section}\n</system-reminder>"
            );

            let compacted = build_compacted_history(CompactedHistoryInput {
                system_message: ConversationItem::system("You are Grok."),
                user_message_prefix: "<user_info>OS: linux</user_info>".into(),
                agents_md_reminder: None,
                state_context: &state_context,
                compaction_summary: "<summary>\n1. Primary Request and Intent: code review\n</summary>"
                    .into(),
                system_reminder: Some(system_reminder),
                summary_before_recent: false,
                transcript_hint: None,
                summary_count: 1,
            });

            let query_texts: Vec<String> = compacted
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::User(u) if u.synthetic_reason.is_human() => {
                        Some(item.text_content())
                    }
                    _ => None,
                })
                .collect();
            assert!(
                query_texts.iter().any(|t| t.contains(&wrap_user_query(DEVICE_TEST_OBJECTIVE))),
                "compacted <user_query> must be the goal objective, not the code review:\n{query_texts:?}"
            );
            assert!(
                query_texts.iter().all(|t| !t.contains(STALE_CODE_REVIEW)),
                "stale pre-goal prompt must not occupy last_user_query:\n{query_texts:?}"
            );

            actor
                .chat_state_handle
                .replace_conversation_for_compaction(compacted);
            let _ = actor.chat_state_handle.get_conversation().await;
            actor.reseed_active_goal_after_compaction().await;

            let after = actor.chat_state_handle.get_conversation().await;
            let goal_summaries: Vec<&ConversationItem> = after
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        ConversationItem::User(u)
                            if u.synthetic_reason == SyntheticReason::GoalSummary
                    )
                })
                .collect();
            assert_eq!(
                goal_summaries.len(),
                1,
                "exactly one GoalSummary continuation after compact: {after:?}"
            );
            let directive = goal_summaries
                .first()
                .expect("len==1")
                .text_content();
            assert!(
                directive.contains(DEVICE_TEST_OBJECTIVE),
                "GoalSummary must carry the live objective:\n{directive}"
            );
            assert!(
                directive.contains(GOAL_CONTINUATION_SENTINEL),
                "GoalSummary must be the next-step continuation:\n{directive}"
            );
            assert!(
                !directive.contains("A goal has been set"),
                "GoalSummary must not be create-style rules:\n{directive}"
            );

            let last_user = after.iter().rev().find(|item| {
                matches!(item, ConversationItem::User(_))
            });
            let last_user = last_user.expect("post-compact history has a user item");
            assert!(
                matches!(
                    last_user,
                    ConversationItem::User(u)
                        if u.synthetic_reason == SyntheticReason::GoalSummary
                ),
                "last cue after compact must be GoalSummary, not create-style rules:\n{:?}",
                last_user.text_content()
            );
            assert!(
                last_user.text_content().contains(GOAL_CONTINUATION_SENTINEL),
                "last cue must continue the goal:\n{}",
                last_user.text_content()
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn paused_goal_does_not_override_later_human_query() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .pause(crate::session::goal_tracker::GoalPauseReason::User)
            );

            assert_eq!(actor.goal_objective_for_compaction(), None);
            assert_eq!(actor.merge_goal_compaction_user_context(None), None);

            let conversation = stale_pre_goal_conversation();
            let ctx = CompactionStateContext::build(
                &conversation,
                CompactionInputs {
                    goal_objective: actor.goal_objective_for_compaction(),
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(
                ctx.last_user_query.as_deref(),
                Some(STALE_CODE_REVIEW),
                "paused goal must not replace the later human prompt"
            );

            let section = actor
                .compaction_goal_section()
                .await
                .expect("paused goal still belongs in the reminder");
            assert!(
                section.contains(DEVICE_TEST_OBJECTIVE),
                "reminder still pins the live objective:\n{section}"
            );
            assert!(
                !section.contains(GOAL_CONTINUATION_SENTINEL),
                "paused reminder must not be a continue-working cue:\n{section}"
            );

            actor.chat_state_handle.replace_conversation(conversation);
            let _ = actor.chat_state_handle.get_conversation().await;
            actor.reseed_active_goal_after_compaction().await;
            let after = actor.chat_state_handle.get_conversation().await;
            assert!(
                after.iter().all(|item| {
                    !matches!(
                        item,
                        ConversationItem::User(u)
                            if u.synthetic_reason == SyntheticReason::GoalSummary
                    )
                }),
                "paused compact must not inject GoalSummary: {after:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn compact_reseed_skips_turn_end_drain_and_round_count() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_goal_actor().await;
            start_device_test_goal(&actor);
            {
                let mut tracker = actor.goal_tracker.lock();
                tracker.snapshot_mut().expect("goal").rounds_since_verify = 4;
            }
            actor.pending_classifier_completions.lock().push_back(
                distill_tools::implementations::distill::update_goal::UpdateGoalInput {
                    completed: None,
                    message: Some("progress".into()),
                    blocked_reason: None,
                },
            );

            actor.reseed_active_goal_after_compaction().await;

            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .expect("goal")
                    .rounds_since_verify,
                4,
                "compact reseed must not count as a worker round"
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                1,
                "compact reseed must not TurnEnd-drain pending classifier completions"
            );

            let after = actor.chat_state_handle.get_conversation().await;
            assert!(
                after.iter().any(|item| {
                    matches!(
                        item,
                        ConversationItem::User(u)
                            if u.synthetic_reason == SyntheticReason::GoalSummary
                    ) && item.text_content().contains(GOAL_CONTINUATION_SENTINEL)
                }),
                "compact still injects a continuation: {after:?}"
            );
        })
        .await;
}

/// A git worktree with an uncommitted edit and its recorded baseline.
fn edited_delivery(tmp: &TempDir) -> crate::session::goal_evaluator::GoalVerificationTarget {
    let delivery = tmp.path().join("delivery");
    std::fs::create_dir(&delivery).unwrap();
    std::fs::write(delivery.join("cors.rs"), "before\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "baseline",
        ],
    ] {
        assert!(
            std::process::Command::new(crate::util::subprocess::git_bin())
                .args(args)
                .current_dir(&delivery)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let head = std::process::Command::new(crate::util::subprocess::git_bin())
        .args(["rev-parse", "HEAD"])
        .current_dir(&delivery)
        .output()
        .unwrap();
    std::fs::write(delivery.join("cors.rs"), "after\n").unwrap();
    crate::session::goal_evaluator::GoalVerificationTarget {
        workspace_root: delivery.to_string_lossy().into_owned(),
        baseline_commit: String::from_utf8(head.stdout).unwrap().trim().to_owned(),
    }
}

fn evaluation_log(tmp: &TempDir) -> Vec<serde_json::Value> {
    std::fs::read_to_string(tmp.path().join("goal/evaluations.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A checkpoint that sees new work and no pending blocker costs no model
/// call: new work is all a checkpoint evaluation would record. The same work
/// is never credited twice, so a worker that stops after one edit still meets
/// a real evaluation at the next checkpoint.
#[tokio::test(flavor = "current_thread")]
async fn a_checkpoint_with_new_work_makes_no_model_call() {
    use super::rate_limit_backoff_tests::{SessionKind, actor_under_test, sampler_surfaces_429};
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let (actor, _) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
            let tmp = TempDir::new().unwrap();
            *actor.goal_tracker.lock() =
                crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf());
            set_goal_harness_for_tests(&actor);
            start_device_test_goal(&actor);
            let target = edited_delivery(&tmp);
            {
                let mut tracker = actor.goal_tracker.lock();
                let goal = tracker.snapshot_mut().unwrap();
                goal.progress.verification_target = Some(target);
                goal.progress.no_progress_rounds = 1;
            }
            assert!(matches!(
                actor.run_goal_progress_checkpoint().await,
                GoalRoundDecision::Continue(_)
            ));
            assert_eq!(
                server.request_count_for("/v1/responses"),
                0,
                "no model call"
            );
            {
                let tracker = actor.goal_tracker.lock();
                let goal = tracker.snapshot().unwrap();
                assert_eq!(goal.progress.no_progress_rounds, 0, "new work is progress");
                assert!(
                    goal.seen_work.is_empty(),
                    "the main evaluator still weighs it"
                );
                assert!(!goal.credited_work.is_empty());
            }

            let verdict = serde_json::json!({
                "decision": "continue", "evidence": "same edit as before",
                "next_step": "run the tests", "blocker_key": "", "blocker_kind": "",
                "progress_evidence": "", "needs_review_panel": false,
                "observations": [], "verification_target": null, "criteria": []
            });
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact(&verdict.to_string(), "test")),
            );
            assert!(matches!(
                actor.run_goal_progress_checkpoint().await,
                GoalRoundDecision::Continue(_)
            ));
            assert_eq!(
                server.request_count_for("/v1/responses"),
                1,
                "credited work is not new again"
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .progress
                    .no_progress_rounds,
                1
            );
            let log = evaluation_log(&tmp);
            assert_eq!(log[0]["evaluator"], "skipped");
            assert_eq!(log[0]["workspace_changed"], true);
            assert_eq!(log[1]["evaluator"], "main");
        })
        .await;
}

/// A checkpoint the harness cannot settle runs on main even with a utility
/// configured: without new harness work only main can tell progress from a
/// stall (a utility observation would reset the stall count unchecked), and a
/// pending blocker is main's to confirm, which may pause the goal.
#[tokio::test(flavor = "current_thread")]
async fn a_checkpoint_the_harness_cannot_settle_runs_on_main() {
    use super::rate_limit_backoff_tests::{SessionKind, actor_under_test, sampler_surfaces_429};
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, ScriptedResponse};

    tokio::task::LocalSet::new().run_until(async {
        let server = MockInferenceServer::start().await.unwrap();
        let utility = MockInferenceServer::start().await.unwrap();
        let (actor, _) = actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
        let tmp = TempDir::new().unwrap();
        *actor.goal_tracker.lock() =
            crate::session::goal_tracker::GoalTracker::new(tmp.path().to_path_buf());
        set_goal_harness_for_tests(&actor);
        start_device_test_goal(&actor);
        {
            let mut tracker = actor.goal_tracker.lock();
            let goal = tracker.snapshot_mut().unwrap();
            goal.progress.criteria.push(serde_json::from_value(serde_json::json!({
                "id":"register", "requirement":"register a new user", "source":"user: prove registration",
                "status":"pending", "evidence":"", "scope":"", "invalidated_by":""
            })).unwrap());
        }
        let verdict = serde_json::json!({
            "decision": "continue", "evidence": "e2e/register passed",
            "next_step": "deploy", "blocker_key": "", "blocker_kind": "",
            "progress_evidence": "", "needs_review_panel": false,
            "observations": [], "verification_target": null, "criteria": []
        })
        .to_string();
        let main_reply = || ScriptedResponse::sse(responses_api_script_exact(&verdict, "test"));
        crate::session::goal_evaluator::set_test_goal_utility(Some(utility.url()));

        // No new work: main judges whether the goal stalled.
        server.enqueue_response("/v1/responses", main_reply());
        assert!(matches!(actor.run_goal_progress_checkpoint().await, GoalRoundDecision::Continue(_)));
        assert_eq!(server.request_count_for("/v1/responses"), 1);

        // New work with a blocker pending: main may confirm the blocker.
        let target = edited_delivery(&tmp);
        {
            let mut tracker = actor.goal_tracker.lock();
            let goal = tracker.snapshot_mut().unwrap();
            goal.progress.verification_target = Some(target);
            goal.evaluator_blocker_key = Some("no_access".into());
        }
        server.enqueue_response("/v1/responses", main_reply());
        actor.run_goal_progress_checkpoint().await;
        crate::session::goal_evaluator::set_test_goal_utility(None);
        assert_eq!(server.request_count_for("/v1/responses"), 2);
        assert_eq!(utility.request_count_for("/v1/chat/completions"), 0, "never asked");
        let log = evaluation_log(&tmp);
        let evaluators: Vec<&str> = log.iter().map(|r| r["evaluator"].as_str().unwrap()).collect();
        assert_eq!(evaluators, ["main", "main"]);
    }).await;
}
