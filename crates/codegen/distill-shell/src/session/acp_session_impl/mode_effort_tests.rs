// Modified for Distill by Samuel Fajreldines, 2026.
//! Each mode's v2 row against its today row in the same scenario: the today
//! row must ask the same Jev questions and apply the same effort as before,
//! and the v2 row must enforce its rule. No model is called: Jev answers come
//! from the scripted queue and main requests from the mock server.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use distill_sampling_types::{ApiBackend, ReasoningEffort};
use distill_tool_types::{EffortCadence, ExecutionMode, V2Modes};
use distill_workspace::jev::JevAnswerSet;
use distill_workspace::jev::catalog::routing;

use super::super::mode_policy::{set_v2_modes_for_tests, take_policy_rows};
use super::*;

/// Every mode on its today row: what the code did before the v2 rows.
fn today() -> Option<V2Modes> {
    Some(V2Modes::default())
}

/// The three Ultra modes on their v2 rows, as with no `[modes] v2` key.
fn v2() -> Option<V2Modes> {
    Some(
        V2Modes::default()
            .with(ExecutionMode::Ultrasave)
            .with(ExecutionMode::Ultrafast)
            .with(ExecutionMode::Ultracode),
    )
}

/// A test actor in `mode` whose model `test` offers `efforts` on a backend
/// whose cache is keyed by effort, with auto effort on and `session_effort`
/// as the configured level.
async fn actor_with_menu(
    mode: ExecutionMode,
    efforts: &[&str],
    session_effort: Option<ReasoningEffort>,
) -> SessionActor {
    let actor = super::super::support::plain_actor().await;
    let mut entry = crate::agent::config::ModelEntry::fallback(
        "test",
        &crate::agent::config::EndpointsConfig::default(),
    );
    entry.info.api_backend = ApiBackend::Responses;
    entry.info.reasoning_efforts = efforts
        .iter()
        .map(|id| distill_sampling_types::ReasoningEffortOption {
            id: (*id).to_owned(),
            value: id.parse().expect("canonical effort"),
            label: (*id).to_owned(),
            description: None,
            default: false,
        })
        .collect();
    entry.api_key = Some("test-key".to_owned());
    actor.models_manager.insert_test_entry("test", entry);
    let mut config = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor has sampling config");
    config.api_backend = ApiBackend::Responses;
    config.reasoning_effort = session_effort;
    actor.chat_state_handle.update_sampling_config(config);
    actor.jev_effort_auto.store(true, Ordering::Relaxed);
    actor.execution_mode.store(mode);
    actor
        .chat_state_handle
        .push_user_message_and_ack(ConversationItem::user("Fix the failing parser test"))
        .await
        .expect("record the request");
    actor
}

/// One effort answer: `mass` is the probability per level; the choice is
/// the heaviest level.
fn effort_answer(mass: &[(&str, f64)]) -> JevAnswerSet {
    let choice = mass
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| (*id).to_owned())
        .unwrap_or_default();
    JevAnswerSet {
        model: "test-jev".to_owned(),
        answers: BTreeMap::from([(
            routing::MICRO_EFFORT_QUESTION.to_owned(),
            distill_workspace::jev::Answer::Choice {
                choice,
                probabilities: mass.iter().map(|(id, p)| ((*id).to_owned(), *p)).collect(),
                confidence: Some(0.9),
            },
        )]),
        usage: Default::default(),
        request_id: None,
        latency_ms: 0,
    }
}

/// The model-tier question's answer that lowers the effort.
fn cheap_tier_answer() -> JevAnswerSet {
    JevAnswerSet {
        model: "test-jev".to_owned(),
        answers: BTreeMap::from([(
            "model_tier".to_owned(),
            distill_workspace::jev::Answer::Choice {
                choice: "cheap".to_owned(),
                probabilities: Default::default(),
                confidence: Some(0.95),
            },
        )]),
        usage: Default::default(),
        request_id: None,
        latency_ms: 0,
    }
}

/// One round's preparation inside `session`'s scope, so policy rows land on it.
async fn prepare(actor: &SessionActor, session: &str) -> SamplingConfig {
    crate::jev::with_session_scope_and_recorder(session, None, actor.prepare_sampler_for_turn())
        .await
}

/// What a main response leaves for the next round: the round ran, and its
/// prompt was `prompt_tokens` long.
fn round_answered(actor: &SessionActor, effort: &str, prompt_tokens: u64) {
    let mut ledger = actor.jev_ledger.borrow_mut();
    ledger.note_round("test", Some(effort.to_owned()));
    ledger.add_usage(prompt_tokens, 10);
}

fn failed_edit() -> distill_tools::types::output::ToolOutput {
    distill_tools::types::output::ToolOutput::SearchReplace(
        distill_tools::types::output::SearchReplaceOutput::FileNotFound("parser.rs".to_owned()),
    )
}

fn rows(session: &str) -> BTreeMap<String, u64> {
    take_policy_rows(session)
}

/// Runs `body` on a LocalSet with `modes` selected, and puts the thread back
/// on the configured rows and an empty Jev queue afterwards.
async fn with_rows<F: std::future::Future<Output = ()>>(modes: Option<V2Modes>, body: F) {
    set_v2_modes_for_tests(modes);
    tokio::task::LocalSet::new().run_until(body).await;
    crate::jev::clear_test_decision_answers();
    let _ = crate::jev::take_test_asked_bodies();
    let _ = crate::jev::take_test_ask_log();
    set_v2_modes_for_tests(None);
}

/// UltraSave v2 offers no level above high and picks at its 0.4 quantile;
/// the today row offers the whole menu and picks at 0.5.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn ultrasave_v2_offers_up_to_high_and_picks_at_its_quantile() {
    // With xhigh counted, the 0.4 point would land on high: only the menu
    // filter brings it down to low.
    let split_to_xhigh = [("low", 0.35), ("high", 0.30), ("xhigh", 0.35)];
    // Low and high equally likely: 0.4 lands on low, 0.5 on medium.
    let split = [("low", 0.45), ("medium", 0.10), ("high", 0.45)];
    for (modes, expected, xhigh_offered) in [
        (
            today(),
            [ReasoningEffort::High, ReasoningEffort::Medium],
            true,
        ),
        (v2(), [ReasoningEffort::Low, ReasoningEffort::Low], false),
    ] {
        with_rows(modes, async {
            for (mass, expected) in [
                (&split_to_xhigh[..], expected[0]),
                (&split[..], expected[1]),
            ] {
                let actor = actor_with_menu(
                    ExecutionMode::Ultrasave,
                    &["low", "medium", "high", "xhigh"],
                    None,
                )
                .await;
                crate::jev::set_test_decision_answers([Some(effort_answer(mass))]);
                let prepared = prepare(&actor, "mode-effort-ultrasave-quantile").await;
                assert_eq!(
                    prepared.reasoning_effort,
                    Some(expected),
                    "{modes:?} {mass:?}"
                );
                let bodies = crate::jev::take_test_asked_bodies();
                assert_eq!(bodies.len(), 1, "one effort question per round");
                assert_eq!(bodies[0].contains("xhigh"), xhigh_offered, "{modes:?}");
            }
            let _ = rows("mode-effort-ultrasave-quantile");
        })
        .await;
    }
}

/// A configured level above the ceiling is lowered to the highest level the
/// menu has under it and recorded; the today row leaves it. A climb of the
/// escalation ratchet is applied after the ceiling, so it is never capped.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn the_ceiling_lowers_a_configured_level_but_not_an_escalated_round() {
    let session = "mode-effort-ceiling";
    for (modes, expected, recorded) in [
        (today(), ReasoningEffort::Xhigh, None),
        (v2(), ReasoningEffort::High, Some(1)),
    ] {
        with_rows(modes, async {
            // The today row offers both levels and Jev picks xhigh; under the
            // ceiling one level is left, so nothing is asked and the
            // configured xhigh would stand.
            let actor = actor_with_menu(
                ExecutionMode::Ultrasave,
                &["high", "xhigh"],
                Some(ReasoningEffort::Xhigh),
            )
            .await;
            crate::jev::set_test_decision_answers([Some(effort_answer(&[("xhigh", 1.0)]))]);
            let prepared = prepare(&actor, session).await;
            assert_eq!(prepared.reasoning_effort, Some(expected), "{modes:?}");
            assert_eq!(
                rows(session).get("ultrasave:effort_ceiling").copied(),
                recorded
            );
        })
        .await;
    }
    with_rows(v2(), async {
        super::super::escalation::set_live_triggers_for_tests(Some(&[
            super::super::escalation::EscalationTrigger::IdenticalCalls,
        ]));
        let actor =
            actor_with_menu(ExecutionMode::Ultrafast, &["low", "medium", "high"], None).await;
        actor.jev_ledger.borrow_mut().last_execution =
            Some(("test".to_owned(), Some(ReasoningEffort::Medium)));
        actor.note_escalation(super::super::escalation::EscalationTrigger::IdenticalCalls);
        crate::jev::set_test_decision_answers([None]);
        let prepared = prepare(&actor, session).await;
        assert_eq!(
            prepared.reasoning_effort,
            Some(ReasoningEffort::High),
            "the ratchet's level stands above UltraFast's medium ceiling"
        );
        super::super::escalation::set_live_triggers_for_tests(None);
        let _ = rows(session);
    })
    .await;
}

/// UltraFast v2 asks its one effort question of the turn on a detached task:
/// the first round runs at the silent level without waiting and records the
/// skipped wait; a later round that keeps the route takes the answer, capped
/// at medium. The today row waits for the answer in the first round.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn ultrafast_v2_takes_one_decision_per_turn_without_waiting() {
    let session = "mode-effort-ultrafast-turn";
    with_rows(today(), async {
        let actor =
            actor_with_menu(ExecutionMode::Ultrafast, &["low", "medium", "high"], None).await;
        let _ = crate::jev::take_test_ask_log();
        crate::jev::set_test_decision_answers([Some(effort_answer(&[("medium", 1.0)]))]);
        let first = prepare(&actor, session).await;
        assert_eq!(first.reasoning_effort, Some(ReasoningEffort::Medium));
        let log = crate::jev::take_test_ask_log();
        assert_eq!(log.len(), 1);
        assert!(!log[0].detached && log[0].dispatched_before == 0, "{log:?}");
        assert!(rows(session).is_empty());
    })
    .await;
    with_rows(v2(), async {
        let actor =
            actor_with_menu(ExecutionMode::Ultrafast, &["low", "medium", "high"], None).await;
        let _ = crate::jev::take_test_ask_log();
        crate::jev::set_test_decision_answers([Some(effort_answer(&[("medium", 1.0)]))]);
        let first = prepare(&actor, session).await;
        assert_eq!(first.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(
            rows(session),
            BTreeMap::from([("ultrafast:jev_wait_skipped".to_owned(), 1)])
        );
        tokio::task::yield_now().await;
        let log = crate::jev::take_test_ask_log();
        assert_eq!(log.len(), 1);
        assert!(log[0].detached, "{log:?}");
        round_answered(&actor, "low", 2_000);
        let second = prepare(&actor, session).await;
        assert_eq!(second.reasoning_effort, Some(ReasoningEffort::Medium));
        round_answered(&actor, "medium", 2_400);
        actor
            .jev_ledger
            .borrow_mut()
            .facts
            .note_tool_result("c1", &failed_edit());
        let third = prepare(&actor, session).await;
        assert_eq!(
            third.reasoning_effort,
            Some(ReasoningEffort::Medium),
            "a failed tool does not reopen the turn's decision"
        );
        assert!(
            crate::jev::take_test_ask_log().is_empty(),
            "one question per turn"
        );
        assert!(rows(session).is_empty());
    })
    .await;
    // Above the ceiling with nothing to ask: lowered to medium.
    with_rows(v2(), async {
        let actor = actor_with_menu(
            ExecutionMode::Ultrafast,
            &["medium", "high", "xhigh"],
            Some(ReasoningEffort::Xhigh),
        )
        .await;
        crate::jev::set_test_decision_answers([]);
        let prepared = prepare(&actor, session).await;
        assert_eq!(prepared.reasoning_effort, Some(ReasoningEffort::Medium));
        assert_eq!(rows(session).get("ultrafast:effort_ceiling"), Some(&1));
    })
    .await;
}

/// The control arm puts UltraFast's v2 row back on the per-round cadence and
/// keeps every other v2 value.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn the_control_arm_keeps_per_round_effort_in_ultrafast_v2() {
    with_rows(v2(), async {
        let actor =
            actor_with_menu(ExecutionMode::Ultrafast, &["low", "medium", "high"], None).await;
        assert_eq!(actor.round_effort_policy().cadence, EffortCadence::PerTurn);
        super::set_ultrafast_effort_per_round_for_tests(Some(true));
        let policy = actor.round_effort_policy();
        assert_eq!(
            policy.cadence,
            distill_tool_types::ModePolicy::today(ExecutionMode::Ultrafast)
                .runtime
                .effort
                .cadence
        );
        assert_eq!(
            policy.ceiling,
            Some(distill_tool_types::EffortLevel::Medium)
        );
        crate::jev::set_test_decision_answers([Some(effort_answer(&[("medium", 1.0)]))]);
        let prepared = prepare(&actor, "mode-effort-control-arm").await;
        assert_eq!(prepared.reasoning_effort, Some(ReasoningEffort::Medium));
        assert!(
            !crate::jev::take_test_ask_log()[0].detached,
            "the arm waits per round"
        );
        super::set_ultrafast_effort_per_round_for_tests(None);
        let _ = rows("mode-effort-control-arm");
    })
    .await;
}

/// UltraCode v2 picks at 0.75, never asks the lowering pass, falls back to
/// the turn's last answered level (else medium) when Jev is silent, and runs
/// at least at high on the round after a failure. The today row picks at
/// 0.5, lowers on a cheap tier answer and falls back to high.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn ultracode_v2_leans_up_and_keeps_its_last_answer_when_jev_is_silent() {
    let session = "mode-effort-ultracode";
    let menu = ["low", "medium", "high"];
    let split = [("low", 0.3), ("medium", 0.3), ("high", 0.4)];
    for (modes, picked, questions) in [
        (
            today(),
            ReasoningEffort::Low,
            vec![
                vec![routing::MICRO_EFFORT_QUESTION.to_owned()],
                vec!["model_tier".to_owned()],
            ],
        ),
        (
            v2(),
            ReasoningEffort::High,
            vec![vec![routing::MICRO_EFFORT_QUESTION.to_owned()]],
        ),
    ] {
        with_rows(modes, async {
            let actor = actor_with_menu(ExecutionMode::Ultracode, &menu, None).await;
            let _ = crate::jev::take_test_asked_questions();
            crate::jev::set_test_decision_answers([
                Some(effort_answer(&split)),
                Some(cheap_tier_answer()),
            ]);
            let prepared = prepare(&actor, session).await;
            assert_eq!(prepared.reasoning_effort, Some(picked), "{modes:?}");
            assert_eq!(crate::jev::take_test_asked_questions(), questions);
            let _ = rows(session);
        })
        .await;
    }
    for (modes, silent_first, silent_after_answer) in [
        (today(), ReasoningEffort::High, ReasoningEffort::High),
        (v2(), ReasoningEffort::Medium, ReasoningEffort::Low),
    ] {
        with_rows(modes, async {
            let actor = actor_with_menu(ExecutionMode::Ultracode, &menu, None).await;
            crate::jev::set_test_decision_answers([None]);
            let first = prepare(&actor, session).await;
            assert_eq!(first.reasoning_effort, Some(silent_first), "{modes:?}");
            round_answered(&actor, "high", 2_000);
            crate::jev::set_test_decision_answers([
                Some(effort_answer(&[("low", 1.0)])),
                None,
                None,
            ]);
            let answered = prepare(&actor, session).await;
            assert_eq!(answered.reasoning_effort, Some(ReasoningEffort::Low));
            round_answered(&actor, "low", 2_200);
            crate::jev::set_test_decision_answers([None, None]);
            let silent = prepare(&actor, session).await;
            assert_eq!(
                silent.reasoning_effort,
                Some(silent_after_answer),
                "{modes:?}"
            );
            let _ = rows(session);
        })
        .await;
    }
    for (modes, after_failure, recorded) in [
        (today(), ReasoningEffort::Low, None),
        (v2(), ReasoningEffort::High, Some(&1)),
    ] {
        with_rows(modes, async {
            let actor = actor_with_menu(ExecutionMode::Ultracode, &menu, None).await;
            crate::jev::set_test_decision_answers([Some(effort_answer(&[("low", 1.0)])), None]);
            let _ = prepare(&actor, session).await;
            round_answered(&actor, "low", 2_000);
            actor
                .jev_ledger
                .borrow_mut()
                .facts
                .note_tool_result("c1", &failed_edit());
            crate::jev::set_test_decision_answers([Some(effort_answer(&[("low", 1.0)])), None]);
            let next = prepare(&actor, session).await;
            assert_eq!(next.reasoning_effort, Some(after_failure), "{modes:?}");
            assert_eq!(rows(session).get("ultracode:effort_floor"), recorded);
            round_answered(&actor, "high", 2_400);
            crate::jev::set_test_decision_answers([Some(effort_answer(&[("low", 1.0)])), None]);
            let later = prepare(&actor, session).await;
            assert_eq!(
                later.reasoning_effort,
                Some(ReasoningEffort::Low),
                "the floor holds for the round after the failure only"
            );
            let _ = rows(session);
        })
        .await;
    }
}

/// A round whose only call waited on background work, and the round that
/// answers the final-check reminder, ask no effort question on a v2 row and
/// keep the last level; the today row asks as usual.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn idle_rounds_keep_the_last_level_on_v2_rows() {
    let session = "mode-effort-idle";
    let wait = vec![
        ConversationItem::user("Run the suite and report"),
        ConversationItem::assistant_tool_calls(vec![distill_sampling_types::ToolCall {
            id: "w1".into(),
            name: "get_command_or_subagent_output".to_owned(),
            arguments: r#"{"task_ids":["t1"],"timeout_ms":120000}"#.into(),
        }]),
        ConversationItem::tool_result("w1", "still running"),
    ];
    let final_check = vec![
        ConversationItem::user("Run the suite and report"),
        ConversationItem::assistant("All tests pass."),
        ConversationItem::system_reminder(super::super::turn::JEV_FINAL_CHECK_REMINDER),
    ];
    for mode in [ExecutionMode::Ultrasave, ExecutionMode::Ultracode] {
        for conversation in [&wait, &final_check] {
            for (modes, expected, asked) in [
                (today(), ReasoningEffort::High, true),
                (v2(), ReasoningEffort::Medium, false),
            ] {
                with_rows(modes, async {
                    let actor = actor_with_menu(mode, &["low", "medium", "high"], None).await;
                    actor
                        .chat_state_handle
                        .replace_conversation(conversation.clone());
                    actor.jev_ledger.borrow_mut().last_execution =
                        Some(("test".to_owned(), Some(ReasoningEffort::Medium)));
                    let _ = crate::jev::take_test_asked_questions();
                    crate::jev::set_test_decision_answers([
                        Some(effort_answer(&[("high", 1.0)])),
                        None,
                    ]);
                    let prepared = prepare(&actor, session).await;
                    assert_eq!(
                        prepared.reasoning_effort,
                        Some(expected),
                        "{mode:?} {modes:?}"
                    );
                    assert_eq!(
                        !crate::jev::take_test_asked_questions().is_empty(),
                        asked,
                        "{mode:?} {modes:?}"
                    );
                    let kept = rows(session)
                        .get(&format!("{}:effort_kept", mode.id()))
                        .copied();
                    assert_eq!(kept, (!asked).then_some(1), "{mode:?} {modes:?}");
                })
                .await;
            }
        }
    }
}

/// A user-pinned effort turns every effort rule of the row off.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn an_explicit_effort_turns_the_rules_off() {
    let session = "mode-effort-pinned";
    with_rows(v2(), async {
        let actor = actor_with_menu(
            ExecutionMode::Ultrasave,
            &["high", "xhigh"],
            Some(ReasoningEffort::Xhigh),
        )
        .await;
        actor.jev_effort_auto.store(false, Ordering::Relaxed);
        crate::jev::set_test_decision_answers([]);
        let prepared = prepare(&actor, session).await;
        assert_eq!(prepared.reasoning_effort, Some(ReasoningEffort::Xhigh));

        let actor = actor_with_menu(
            ExecutionMode::Ultracode,
            &["low", "medium", "high"],
            Some(ReasoningEffort::Low),
        )
        .await;
        actor.jev_effort_auto.store(false, Ordering::Relaxed);
        round_answered(&actor, "low", 2_000);
        actor
            .jev_ledger
            .borrow_mut()
            .facts
            .note_tool_result("c1", &failed_edit());
        let prepared = prepare(&actor, session).await;
        assert_eq!(prepared.reasoning_effort, Some(ReasoningEffort::Low));

        let actor = actor_with_menu(
            ExecutionMode::Ultrafast,
            &["low", "medium", "high"],
            Some(ReasoningEffort::High),
        )
        .await;
        actor.jev_effort_auto.store(false, Ordering::Relaxed);
        let prepared = prepare(&actor, session).await;
        assert_eq!(prepared.reasoning_effort, Some(ReasoningEffort::High));
        assert!(crate::jev::take_test_ask_log().is_empty());
        assert!(rows(session).is_empty(), "no rule acted");
    })
    .await;
}

/// The late skill line joins the conversation before the turn's second
/// request when it is in; one still in flight then is dropped.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn a_late_skill_hint_is_appended_before_round_two_or_dropped() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let line = "<skill_relevance>\nNo skill in the catalog appears relevant to this request.\n</skill_relevance>";
            for (ready, appended) in [(true, true), (false, false)] {
                let actor = super::super::support::plain_actor().await;
                let turn = actor.current_turn_number.get();
                let hint = if ready {
                    tokio::task::spawn_local(async move { Some(line.to_owned()) })
                } else {
                    tokio::task::spawn_local(std::future::pending::<Option<String>>())
                };
                tokio::task::yield_now().await;
                actor.jev_ledger.borrow_mut().late_skill_hint = Some((turn, hint));
                actor.flush_late_skill_hint(1);
                assert!(
                    actor.jev_ledger.borrow().late_skill_hint.is_some(),
                    "the first request does not take it"
                );
                actor.flush_late_skill_hint(2);
                assert!(actor.jev_ledger.borrow().late_skill_hint.is_none());
                let conversation = actor.chat_state_handle.get_conversation().await;
                let shown = conversation
                    .iter()
                    .any(|item| item.text_content().contains("<skill_relevance>"));
                assert_eq!(shown, appended, "ready: {ready}");
            }
        })
        .await;
}

/// UltraFast v2 waits at most 800 ms for the after-edit review and records
/// the cut; the today row waits the whole Jev deadline.
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[serial_test::serial]
async fn c4_waits_the_rows_deadline() {
    let session = "mode-effort-c4";
    let slow_review = || async {
        tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
        7_u8
    };
    for (modes, answer, recorded) in [(today(), 7, None), (v2(), 0, Some(&1))] {
        with_rows(modes, async {
            let actor = super::super::support::plain_actor().await;
            actor.execution_mode.store(ExecutionMode::Ultrafast);
            let got = crate::jev::with_session_scope_and_recorder(
                session,
                None,
                actor.within_c4_deadline(slow_review()),
            )
            .await;
            assert_eq!(got, answer, "{modes:?}");
            assert_eq!(rows(session).get("ultrafast:c4_deadline"), recorded);
        })
        .await;
    }
}

/// A pinned UltraFast turn against the mock model: on the v2 row no Jev
/// question is awaited before the first main request, which runs at the
/// silent level, and the second request runs at the turn's answer; the today
/// row awaits the effort question before the first request.
#[test]
#[serial_test::serial]
fn a_pinned_ultrafast_turn_awaits_no_jev_call_before_its_first_request() {
    let (today_log, today_efforts) = run_ultrafast_turn(today());
    assert!(
        today_log.iter().any(|ask| !ask.detached
            && ask.dispatched_before == 0
            && ask.lever == "b2_micro_effort"),
        "{today_log:?}"
    );
    assert_eq!(today_efforts, ["medium", "medium"]);
    let (v2_log, v2_efforts) = run_ultrafast_turn(v2());
    assert!(
        v2_log
            .iter()
            .all(|ask| ask.detached || ask.dispatched_before > 0),
        "{v2_log:?}"
    );
    assert!(
        v2_log
            .iter()
            .any(|ask| ask.detached && ask.lever == "b2_micro_effort"),
        "{v2_log:?}"
    );
    assert_eq!(v2_efforts, ["low", "medium"]);
}

/// One pinned UltraFast turn (a tool round, then the answer) on `modes`: the
/// asks the Jev queue answered and the effort each main request carried.
fn run_ultrafast_turn(modes: Option<V2Modes>) -> (Vec<crate::jev::TestAsk>, Vec<String>) {
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
                set_v2_modes_for_tests(modes);
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
                let (actor, _) = super::super::mode_policy::tests::root_actor_for(&server).await;
                let mut entry = crate::agent::config::ModelEntry::fallback(
                    "test",
                    &crate::agent::config::EndpointsConfig::default(),
                );
                entry.info.base_url = server.url();
                entry.info.api_backend = ApiBackend::Responses;
                entry.info.reasoning_efforts = ["low", "medium", "high"]
                    .iter()
                    .map(|id| distill_sampling_types::ReasoningEffortOption {
                        id: (*id).to_owned(),
                        value: id.parse().expect("canonical effort"),
                        label: (*id).to_owned(),
                        description: None,
                        default: false,
                    })
                    .collect();
                entry.api_key = Some("test-key".to_owned());
                actor.models_manager.insert_test_entry("test", entry);
                actor.execution_mode.store(ExecutionMode::Ultrafast);
                actor.jev_effort_auto.store(true, Ordering::Relaxed);
                *actor.current_prompt_id.lock().unwrap() = Some("ultrafast-turn".into());
                let _ = crate::jev::take_test_ask_log();
                crate::jev::set_test_decision_answers(
                    std::iter::once(Some(effort_answer(&[("medium", 1.0)])))
                        .chain(std::iter::repeat_n(None, 63)),
                );
                tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    actor.handle_prompt(
                        "ultrafast-turn",
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
                .expect("turn must finish within timeout")
                .expect("turn must not error");
                let log = crate::jev::take_test_ask_log();
                crate::jev::clear_test_decision_answers();
                set_v2_modes_for_tests(None);
                let efforts: Vec<String> = server
                    .requests()
                    .into_iter()
                    .filter(|entry| entry.path == "/v1/responses")
                    .filter_map(|entry| {
                        entry.body?["reasoning"]["effort"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .collect();
                sender.send((log, efforts)).expect("send result");
            });
        })
        .expect("spawn test thread")
        .join()
        .expect("test thread");
    receiver.recv().expect("turn result")
}
