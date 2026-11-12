// Modified for Distill by Samuel Fajreldines, 2026.
//! The escalation ratchet on a session actor: no model is called; Jev answers
//! are scripted and the reviewer is a scripted subagent coordinator.

use super::*;
use crate::session::acp_session::support::*;
use EscalationTrigger::{C4Break, CheckFailedTwice, IdenticalCalls, RoutedError, StalledRounds};
use distill_tools::implementations::distill::task::types::{SubagentEvent, SubagentResult};
use distill_tools::types::output::{BashOutput, SearchReplaceEditsApplied, SearchReplaceOutput};

/// Clears the per-thread overrides a test set, whatever way it ends.
struct Overrides;

impl Overrides {
    fn live(triggers: &[EscalationTrigger]) -> Self {
        set_live_triggers_for_tests(Some(triggers));
        Self
    }
}

impl Drop for Overrides {
    fn drop(&mut self) {
        set_live_triggers_for_tests(None);
        crate::jev::clear_test_decision_answers();
        crate::jev::clear_test_flags();
    }
}

fn run<F: std::future::Future<Output = ()>>(body: F) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, body);
}

/// Escalation rows recorded for `session`, oldest first.
fn rows(session: &str) -> Vec<String> {
    crate::jev::recorded_decisions_for_test(session)
        .into_iter()
        .filter(|(lever, _)| lever == "escalation")
        .map(|(_, label)| label)
        .collect()
}

async fn actor_in_mode(mode: ExecutionMode) -> SessionActor {
    let actor = plain_actor().await;
    actor.execution_mode.store(mode);
    actor
        .jev_effort_auto
        .store(true, std::sync::atomic::Ordering::Relaxed);
    actor
}

fn check(command: &str, exit_code: i32, output: &str) -> ToolOutput {
    ToolOutput::Bash(BashOutput {
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

/// The same check fails, the tree changes, and it fails again.
fn fail_twice_with_a_change(actor: &SessionActor) {
    let failing = check("cargo test", 101, "test result: FAILED. 0 passed; 1 failed");
    let mut ledger = actor.jev_ledger.borrow_mut();
    for tree in ["tree-1", "tree-2"] {
        ledger.facts.note_tool_result_at(
            "",
            &failing,
            &super::super::turn_facts::CheckContext {
                cwd: "/repo".to_owned(),
                worktree: Some(tree.to_owned()),
            },
        );
    }
}

fn edit_of(path: &str) -> ToolOutput {
    ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(
        SearchReplaceEditsApplied {
            old_string: "a".to_owned(),
            new_string: "b".to_owned(),
            tool_output_for_prompt: String::new(),
            tool_output_for_prompt_concise: None,
            absolute_path: path.into(),
            edits: Default::default(),
            patch: None,
            unicode_normalized: false,
        },
    ))
}

fn report_prompt_tokens(actor: &SessionActor, tokens: u64) {
    let mut ledger = actor.jev_ledger.borrow_mut();
    ledger.note_round("main", None);
    ledger.add_usage(tokens, 1);
}

fn effort_model(actor: &SessionActor) {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use distill_sampling_types::ReasoningEffortOption;
    let mut entry = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts = [
        ("low", ReasoningEffort::Low),
        ("medium", ReasoningEffort::Medium),
        ("high", ReasoningEffort::High),
    ]
    .into_iter()
    .map(|(id, value)| ReasoningEffortOption {
        id: id.to_owned(),
        value,
        label: id.to_owned(),
        description: None,
        default: false,
    })
    .collect();
    actor
        .models_manager
        .insert_test_entry("effort-model", entry);
    actor.jev_ledger.borrow_mut().last_execution =
        Some(("effort-model".to_owned(), Some(ReasoningEffort::Low)));
}

fn mode_answer(choice: &str, probability: f64) -> distill_workspace::jev::JevAnswerSet {
    distill_workspace::jev::JevAnswerSet {
        model: "test-jev".to_owned(),
        answers: [(
            super::super::jev_auto_mode::MODE_QUESTION.to_owned(),
            distill_workspace::jev::Answer::Choice {
                choice: choice.to_owned(),
                probabilities: [(choice.to_owned(), probability)].into(),
                confidence: Some(probability),
            },
        )]
        .into(),
        usage: Default::default(),
        request_id: None,
        latency_ms: 0,
    }
}

/// An unfitted trigger must stay in shadow, so only the names the user lists
/// act; a typo is ignored rather than switching on something else.
#[test]
fn the_config_names_the_live_triggers_and_ignores_the_rest() {
    let root: toml::Value = toml::from_str(
        "[modes]\nescalation = [\"check_failed_twice\", \" c4_break \", \"budget\"]\n\
         [modes.ultracode]\ncontract = \"v2\"\n",
    )
    .unwrap();
    let (live, unknown) = live_triggers_in(Some(&root));
    assert!(live.contains(CheckFailedTwice) && live.contains(C4Break));
    for trigger in [RoutedError, IdenticalCalls, StalledRounds] {
        assert!(!live.contains(trigger), "{}", trigger.id());
    }
    assert_eq!(unknown, vec!["\"budget\"".to_owned()]);
    let (none, unknown) = live_triggers_in(None);
    assert!(
        none.is_empty() && unknown.is_empty(),
        "no key: everything in shadow"
    );
    let empty: toml::Value = toml::from_str("[modes]\nescalation = []\n").unwrap();
    assert!(live_triggers_in(Some(&empty)).0.is_empty());
}

/// The shadow rows are what fits a trigger: one per trigger per turn, so a
/// fire rate counts turns, and none from children, whose turns belong to the
/// root's.
#[test]
fn each_trigger_is_recorded_once_per_turn_and_only_in_root_sessions() {
    run(async {
        let _overrides = Overrides::live(&[]);
        let actor = actor_in_mode(ExecutionMode::Ultrasave).await;
        let all = EscalationTrigger::ALL;
        crate::jev::with_session_scope("escalation-shadow-root", async {
            for trigger in all.into_iter().chain(all) {
                actor.note_escalation(trigger);
            }
            let _ = actor.jev_ledger.borrow_mut().take_rows();
            actor.note_escalation(CheckFailedTwice);
        })
        .await;
        let mut expected: Vec<String> = all
            .iter()
            .map(|trigger| trigger.shadow_label().to_owned())
            .collect();
        expected.push("shadow:check_failed_twice".to_owned());
        assert_eq!(
            rows("escalation-shadow-root"),
            expected,
            "a new turn records again"
        );

        let mut child = actor_in_mode(ExecutionMode::Ultrasave).await;
        child.startup_hints.is_subagent = true;
        crate::jev::with_session_scope("escalation-shadow-child", async {
            for trigger in all {
                child.note_escalation(trigger);
            }
        })
        .await;
        assert!(rows("escalation-shadow-child").is_empty());
    });
}

/// Each trigger is read from the harness's own facts: a repeated check after
/// a change, a C4 verdict on code (not on prose), and a failed routed request.
#[test]
fn harness_facts_fire_their_triggers() {
    run(async {
        let _overrides = Overrides::live(&[]);
        let actor = actor_in_mode(ExecutionMode::Ultrasave).await;
        let session = "escalation-facts";
        crate::jev::with_session_scope(session, async {
            actor.escalation_after_round().await;
            assert!(rows(session).is_empty(), "no facts, no trigger");
            fail_twice_with_a_change(&actor);
            actor.escalation_after_round().await;
            assert_eq!(rows(session), vec!["shadow:check_failed_twice"]);

            let cwd = actor.tool_context.cwd.as_path().to_path_buf();
            for prose in ["README.md", "docs/guide.rs", "notes.txt", "a.rst", "b.mdx"] {
                let path = cwd.join(prose).display().to_string();
                actor.note_c4_escalation(DiffReviewVerdict::Breaks, Some(0.9), &edit_of(&path));
            }
            let code = cwd.join("src/lib.rs").display().to_string();
            actor.note_c4_escalation(DiffReviewVerdict::Ok, Some(0.9), &edit_of(&code));
            actor.note_c4_escalation(DiffReviewVerdict::Breaks, None, &edit_of(&code));
            assert_eq!(
                rows(session).len(),
                1,
                "prose, a pass and a deferral fire nothing"
            );
            actor.note_c4_escalation(DiffReviewVerdict::Incomplete, Some(0.7), &edit_of(&code));
            assert_eq!(
                rows(session).last().map(String::as_str),
                Some("shadow:c4_break")
            );

            let error = distill_sampler::SamplingErrorInfo {
                kind: distill_sampler::SamplingErrorKind::Api,
                status_code: Some(500),
                message: "upstream failed".to_owned(),
                is_retryable: false,
                retry_after_secs: None,
                should_retry: None,
                error_code: None,
                model_metadata: None,
                empty_response_context: None,
                doom_loop_triggers: None,
                doom_loop_aborted_at_chunk: None,
                credential: distill_sampling_types::SentCredential::Unknown,
            };
            let mut request = distill_sampling_types::ConversationRequest::default();
            actor.undo_local_route(&mut request, &error).await;
            assert_eq!(
                rows(session).len(),
                2,
                "a round on the session model is not routed"
            );
            actor.jev_ledger.borrow_mut().tier_route_active = true;
            actor.undo_local_route(&mut request, &error).await;
            assert_eq!(
                rows(session).last().map(String::as_str),
                Some("shadow:routed_error")
            );
        })
        .await;
    });
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new(crate::util::subprocess::git_bin())
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/c.rs"), "fn c() {}\n").unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "base"]);
    dir
}

/// Rounds that change nothing and pass nothing are the stall signal; a new
/// change in the tree starts the count again. A round is judged one round
/// late, so the turn never waits on git.
#[test]
fn stalled_rounds_fire_after_quiet_rounds_and_a_change_resets_them() {
    run(async {
        let _overrides = Overrides::live(&[]);
        let repo = repo();
        let mut actor = actor_in_mode(ExecutionMode::Ultrasave).await;
        actor.tool_context.cwd = distill_paths::AbsPathBuf::new(repo.path().to_path_buf()).unwrap();
        let session = "escalation-stall";
        // Lets the marker a round started finish, so it shows the tree as the
        // round left it.
        let actor = &actor;
        let round = move || async move {
            actor.escalation_after_round().await;
            while !actor
                .jev_ledger
                .borrow()
                .escalation
                .stall
                .pending
                .as_ref()
                .is_none_or(|(handle, _)| handle.is_finished())
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        let quiet = move || actor.jev_ledger.borrow().escalation.stall.quiet_rounds;
        crate::jev::with_session_scope(session, async {
            for _ in 0..5 {
                round().await;
            }
            assert_eq!(quiet(), 3, "the first two rounds only set the baseline");
            std::fs::write(repo.path().join("src/c.rs"), "fn c() { 1 }\n").unwrap();
            round().await;
            round().await;
            assert_eq!(
                quiet(),
                0,
                "the round that changed the tree resets the count"
            );
            for _ in 1..STALLED_ROUNDS {
                round().await;
            }
            assert!(rows(session).is_empty());
            round().await;
            assert_eq!(rows(session), vec!["shadow:stalled_rounds"]);
            for _ in 0..STALLED_ROUNDS {
                round().await;
            }
            assert_eq!(rows(session).len(), 1, "once per turn");
        })
        .await;
    });
}

/// With `[modes] escalation` empty (the default) the triggers are only
/// recorded: no effort, mode, model or tier changes, and Auto's review moves
/// the mode down as freely as it does without the ratchet.
#[test]
fn with_no_live_trigger_only_the_shadow_rows_change() {
    run(async {
        let _overrides = Overrides::live(&[]);
        crate::jev::set_test_flags(distill_workspace::jev::JevFlags::harness_default());
        let actor = actor_in_mode(ExecutionMode::Ultrafast).await;
        effort_model(&actor);
        report_prompt_tokens(&actor, 200_000);
        {
            let mut ledger = actor.jev_ledger.borrow_mut();
            ledger.tier_route_active = true;
            ledger.last_execution = Some(("worker-model".to_owned(), None));
        }
        for trigger in EscalationTrigger::ALL {
            actor.note_escalation(trigger);
        }
        {
            let ledger = actor.jev_ledger.borrow();
            let state = &ledger.escalation;
            assert_eq!((state.climbs, state.escalations()), (0, 0));
            assert!(state.effort_level.is_none() && state.tier_floor.is_none());
            assert_eq!(state.review, ReviewStep::None);
        }
        assert_eq!(actor.escalation_effort_mode(), ExecutionMode::Ultrafast);
        let mut cfg = SamplingConfig {
            model: "effort-model".into(),
            reasoning_effort: Some(ReasoningEffort::Low),
            ..Default::default()
        };
        actor.escalation_apply_effort_floor(&mut cfg);
        assert_eq!(cfg.reasoning_effort, Some(ReasoningEffort::Low));
        let entries = [
            TierEntry {
                tier: Tier::Utility,
                model: "utility-model".into(),
            },
            TierEntry {
                tier: Tier::Worker,
                model: "worker-model".into(),
            },
        ];
        assert_eq!(
            actor.escalation_refused_switch(Tier::Utility, &entries),
            None,
            "the mid-turn refusal is part of the opt-in"
        );

        let auto = plain_actor().await;
        auto.execution_mode.store_auto(ExecutionMode::Normal);
        report_prompt_tokens(&auto, 200_000);
        for trigger in EscalationTrigger::ALL {
            auto.note_escalation(trigger);
        }
        assert_eq!(auto.execution_mode.load(), ExecutionMode::Normal);
        crate::jev::set_test_decision_answers([Some(mode_answer("ultrasave", 0.9))]);
        assert!(auto.jev_auto_review_mode("rename a variable", 1).await);
        assert_eq!(auto.execution_mode.load(), ExecutionMode::Ultrasave);
    });
}

/// A live trigger climbs, but a turn climbs at most twice and changes tier at
/// most once, however many signals follow. The ratchet needs no Jev answer.
#[test]
fn a_live_ratchet_climbs_twice_and_steps_tier_once_without_jev() {
    run(async {
        let _overrides = Overrides::live(&EscalationTrigger::ALL);
        crate::jev::set_test_flags(distill_workspace::jev::JevFlags::default());
        crate::jev::set_test_worker_model(Some("worker-slug".to_owned()));
        let actor = actor_in_mode(ExecutionMode::Ultrasave).await;
        effort_model(&actor);
        report_prompt_tokens(&actor, 4_000);
        {
            let mut ledger = actor.jev_ledger.borrow_mut();
            ledger.tier_route_active = true;
            ledger.tier_route_slug = Some("utility-slug".to_owned());
        }
        let session = "escalation-live";
        crate::jev::with_session_scope(session, async {
            fail_twice_with_a_change(&actor);
            actor.escalation_after_round().await;
            {
                let ledger = actor.jev_ledger.borrow();
                let state = &ledger.escalation;
                assert_eq!(state.climbs, 1);
                assert_eq!(
                    state.tier_floor,
                    Some(Tier::Worker),
                    "one above the utility round"
                );
                assert_eq!(
                    state.effort_level.as_ref().map(|(level, _)| level.as_str()),
                    Some("medium")
                );
            }
            let mut cfg = SamplingConfig {
                model: "effort-model".into(),
                reasoning_effort: Some(ReasoningEffort::Low),
                ..Default::default()
            };
            actor.escalation_apply_effort_floor(&mut cfg);
            assert_eq!(
                cfg.reasoning_effort,
                Some(ReasoningEffort::Medium),
                "every later round"
            );
            actor.escalation_apply_effort_floor(&mut cfg);
            assert_eq!(cfg.reasoning_effort, Some(ReasoningEffort::Medium));

            actor.note_escalation(IdenticalCalls);
            actor.note_escalation(StalledRounds);
            actor.note_escalation(RoutedError);
            let ledger = actor.jev_ledger.borrow();
            let state = &ledger.escalation;
            assert_eq!(state.climbs, MAX_CLIMBS);
            assert_eq!(state.review, ReviewStep::Requested);
            assert_eq!(
                state.tier_floor,
                Some(Tier::Worker),
                "the small-prompt takeover would be a second tier step"
            );
            assert_eq!(state.escalations(), 3);
            // The turn outcome record reports the same count.
            assert_eq!(ledger.turn_escalations, 3);
        })
        .await;
        assert_eq!(
            rows(session),
            vec![
                "shadow:check_failed_twice",
                "tier:escalated",
                "climb:effort",
                "shadow:identical_calls",
                "climb:review",
                "shadow:stalled_rounds",
                "shadow:routed_error",
            ]
        );
        crate::jev::clear_test_worker_model();
    });
}

/// The ratchet's state is the turn's: the drain at turn end clears every
/// climb, floor and recorded trigger, so the next turn starts lean again.
#[test]
fn the_ratchet_state_ends_with_the_turn() {
    run(async {
        let _overrides = Overrides::live(&EscalationTrigger::ALL);
        let actor = actor_in_mode(ExecutionMode::Ultrafast).await;
        effort_model(&actor);
        actor.note_escalation(CheckFailedTwice);
        actor.note_escalation(C4Break);
        actor.jev_ledger.borrow_mut().escalation.tier_floor = Some(Tier::Main);
        assert!(actor.jev_ledger.borrow().escalation.escalations() > 0);
        let _ = actor.jev_ledger.borrow_mut().take_rows();
        let ledger = actor.jev_ledger.borrow();
        let state = &ledger.escalation;
        assert_eq!((state.recorded, state.climbs), (0, 0));
        assert!(state.effort_level.is_none() && state.tier_floor.is_none());
        assert!(!state.normal_effort_policy && !state.tier_escalated);
    });
}

/// UltraFast's second climb gives back Normal's effort policy for the rest of
/// the turn; the first only raised the floor.
#[test]
fn ultrafast_second_climb_restores_normal_effort_policy() {
    run(async {
        let _overrides = Overrides::live(&[CheckFailedTwice, IdenticalCalls]);
        let actor = actor_in_mode(ExecutionMode::Ultrafast).await;
        effort_model(&actor);
        actor.note_escalation(CheckFailedTwice);
        assert_eq!(actor.escalation_effort_mode(), ExecutionMode::Ultrafast);
        actor.note_escalation(C4Break);
        assert_eq!(
            actor.escalation_effort_mode(),
            ExecutionMode::Ultrafast,
            "a trigger in shadow does not climb"
        );
        actor.note_escalation(IdenticalCalls);
        assert_eq!(actor.escalation_effort_mode(), ExecutionMode::Normal);
        assert_eq!(
            actor.execution_mode.load(),
            ExecutionMode::Ultrafast,
            "the mode stays"
        );
        assert!(!actor.jev_ledger.borrow().escalation.tier_escalated);
    });
}

/// Under Auto a climb is one mode up, decided without Jev, and the review
/// cannot bring the mode down again inside the request; the next request's
/// pick can.
#[test]
fn under_auto_a_climb_moves_the_mode_up_until_the_next_request() {
    run(async {
        let _overrides = Overrides::live(&[CheckFailedTwice, IdenticalCalls]);
        let actor = plain_actor().await;
        actor.execution_mode.store_auto(ExecutionMode::Ultrasave);
        actor.note_escalation(CheckFailedTwice);
        assert_eq!(actor.execution_mode.load(), ExecutionMode::Normal);
        assert!(actor.execution_mode.is_auto());
        for (round, down) in [(1, "ultrasave"), (2, "ultrafast")] {
            crate::jev::set_test_decision_answers([Some(mode_answer(down, 0.95))]);
            assert!(!actor.jev_auto_review_mode("fix the build", round).await);
            assert_eq!(actor.execution_mode.load(), ExecutionMode::Normal);
        }
        crate::jev::set_test_decision_answers([Some(mode_answer("ultracode", 0.95))]);
        assert!(
            actor.jev_auto_review_mode("fix the build", 3).await,
            "up stays free"
        );
        crate::jev::set_test_decision_answers([Some(mode_answer("normal", 0.95))]);
        assert!(
            actor.jev_auto_review_mode("fix the build", 4).await,
            "Normal is the floor"
        );

        actor.note_escalation(IdenticalCalls);
        assert_eq!(actor.execution_mode.load(), ExecutionMode::Ultracode);
        actor.note_escalation(C4Break);
        assert_eq!(actor.jev_ledger.borrow().escalation.climbs, MAX_CLIMBS);

        let _ = actor.jev_ledger.borrow_mut().take_rows();
        crate::jev::set_test_decision_answers([Some(mode_answer("ultrasave", 0.9))]);
        actor.jev_auto_pick_mode("next request").await;
        assert_eq!(actor.execution_mode.load(), ExecutionMode::Ultrasave);
    });
}

/// The scripted reviewer: answers each spawn with `verdict` and counts the
/// spawns.
fn scripted_reviewer(
    actor: &mut SessionActor,
    verdict: &'static str,
) -> std::rc::Rc<std::cell::Cell<u32>> {
    let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    actor.tool_context.subagent_event_tx = Some(tx);
    let counter = spawns.clone();
    tokio::task::spawn_local(async move {
        while let Some(event) = rx.recv().await {
            let SubagentEvent::Spawn(req) = event else {
                continue;
            };
            counter.set(counter.get() + 1);
            let path = crate::session::goal_classifier::parse_verdict_path_from_prompt(&req.prompt)
                .unwrap();
            tokio::fs::write(path, verdict).await.unwrap();
            let id = req.id.clone();
            let _ = req.result_tx.send(SubagentResult {
                success: true,
                output: std::sync::Arc::from("Refuted"),
                subagent_id: id.clone(),
                child_session_id: id,
                ..Default::default()
            });
        }
    });
    spawns
}

/// UltraSave's second climb runs one review over the turn's own patch and
/// hands its gaps back once; the main model takes over only while the prompt
/// is small enough for the switch to be cheap.
#[test]
fn ultrasave_second_climb_reviews_once_and_takes_over_only_on_a_small_prompt() {
    const REFUTE: &str =
        r#"{"refuted":true,"evidence":"src/c.rs:1 off by one","confidence":"high"}"#;
    for (prompt_tokens, takeover) in [
        (EFFORT_SWITCH_FREE_PROMPT_TOKENS - 1, true),
        (EFFORT_SWITCH_FREE_PROMPT_TOKENS, false),
    ] {
        run(async {
            let _overrides = Overrides::live(&[CheckFailedTwice, C4Break]);
            let repo = repo();
            let mut actor = actor_in_mode(ExecutionMode::Ultrasave).await;
            actor.tool_context.cwd =
                distill_paths::AbsPathBuf::new(repo.path().to_path_buf()).unwrap();
            let spawns = scripted_reviewer(&mut actor, REFUTE);
            actor.escalation_before_round().await;
            std::fs::write(repo.path().join("src/c.rs"), "fn c() { 2 }\n").unwrap();
            report_prompt_tokens(&actor, prompt_tokens);
            actor.note_escalation(CheckFailedTwice);
            actor.note_escalation(C4Break);
            assert_eq!(
                actor.escalation_tier_floor() == Some(Tier::Main),
                takeover,
                "{prompt_tokens} prompt tokens"
            );
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                actor.escalation_after_round().await;
                actor.escalation_after_round().await;
            })
            .await
            .expect("the review must finish");
            assert_eq!(spawns.get(), 1, "one review per turn");
            let reminders: Vec<String> = actor
                .chat_state_handle
                .get_conversation()
                .await
                .iter()
                .map(|item| item.text_content())
                .filter(|text| text.contains("independent read-only review"))
                .collect();
            assert_eq!(reminders.len(), 1, "{reminders:?}");
            assert!(reminders[0].contains("off by one"), "{}", reminders[0]);
        });
    }
}

/// A floor at the main model leaves nothing to choose: no question is asked.
/// A lower floor only drops the tiers under it.
#[test]
fn the_tier_floor_drops_lower_candidates_and_asks_nothing_at_main() {
    let entries = vec![
        TierEntry {
            tier: Tier::Utility,
            model: "u".into(),
        },
        TierEntry {
            tier: Tier::Worker,
            model: "w".into(),
        },
        TierEntry {
            tier: Tier::Main,
            model: "m".into(),
        },
    ];
    assert_eq!(at_or_above_floor(entries.clone(), None), entries);
    assert_eq!(
        at_or_above_floor(entries.clone(), Some(Tier::Worker)),
        entries[1..].to_vec()
    );
    assert_eq!(
        at_or_above_floor(entries.clone(), Some(Tier::Main)),
        entries[2..].to_vec()
    );
    run(async {
        let _overrides = Overrides::live(&[]);
        crate::jev::set_test_flags(distill_workspace::jev::JevFlags {
            b2_execution_tier: true,
            ..distill_workspace::jev::JevFlags::harness_default()
        });
        let actor = actor_in_mode(ExecutionMode::Ultrasave).await;
        actor.jev_ledger.borrow_mut().escalation.tier_floor = Some(Tier::Main);
        let mut cfg = SamplingConfig {
            model: "main".into(),
            ..Default::default()
        };
        crate::jev::set_test_decision_answers([None]);
        crate::jev::with_session_scope("escalation-floor", async {
            assert!(!actor.jev_route_execution_tier(&mut cfg).await);
        })
        .await;
        assert_eq!(cfg.model, "main");
        assert_eq!(
            crate::jev::test_decision_answers_remaining(),
            1,
            "nothing was asked"
        );
        assert_eq!(rows("escalation-floor"), vec!["tier:floor_main"]);
    });
}

/// From 16,384 prompt tokens a switch re-reads the prompt uncached: a
/// mid-turn move to a lower tier is refused and the round keeps its model.
/// Below that, or upward, the switch stands.
#[test]
fn a_midturn_downward_switch_is_refused_from_the_free_switch_limit() {
    let large = Some(EFFORT_SWITCH_FREE_PROMPT_TOKENS);
    let small = Some(EFFORT_SWITCH_FREE_PROMPT_TOKENS - 1);
    assert!(refuses_midturn_switch(
        Some(Tier::Worker),
        Tier::Utility,
        large
    ));
    assert!(refuses_midturn_switch(
        Some(Tier::Main),
        Tier::Worker,
        large
    ));
    assert!(!refuses_midturn_switch(
        Some(Tier::Worker),
        Tier::Utility,
        small
    ));
    assert!(
        !refuses_midturn_switch(Some(Tier::Worker), Tier::Main, large),
        "up is free"
    );
    assert!(!refuses_midturn_switch(
        Some(Tier::Worker),
        Tier::Worker,
        large
    ));
    assert!(
        !refuses_midturn_switch(None, Tier::Utility, large),
        "the turn's first round"
    );
    run(async {
        let _overrides = Overrides::live(&[C4Break]);
        let actor = actor_in_mode(ExecutionMode::Ultrasave).await;
        let entries = [
            TierEntry {
                tier: Tier::Utility,
                model: "utility-model".into(),
            },
            TierEntry {
                tier: Tier::Worker,
                model: "worker-model".into(),
            },
            TierEntry {
                tier: Tier::Main,
                model: "main-model".into(),
            },
        ];
        actor.jev_ledger.borrow_mut().last_execution = Some(("worker-model".to_owned(), None));
        report_prompt_tokens(&actor, EFFORT_SWITCH_FREE_PROMPT_TOKENS);
        crate::jev::with_session_scope("escalation-refusal", async {
            assert_eq!(
                actor.escalation_refused_switch(Tier::Utility, &entries),
                Some(Tier::Worker)
            );
            assert_eq!(actor.escalation_refused_switch(Tier::Main, &entries), None);
            report_prompt_tokens(&actor, EFFORT_SWITCH_FREE_PROMPT_TOKENS - 1);
            assert_eq!(
                actor.escalation_refused_switch(Tier::Utility, &entries),
                None
            );
        })
        .await;
        assert_eq!(rows("escalation-refusal"), vec!["tier:refused_midturn"]);
    });
}
