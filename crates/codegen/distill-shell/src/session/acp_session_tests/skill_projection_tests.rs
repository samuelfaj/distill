// Modified for Distill by Samuel Fajreldines, 2026.
//! Actor-level tests for the request-aware skill listing (Jev P6), the
//! per-turn `<skill_relevance>` hint and the turn-start pass (P1 families, B6
//! delegation).
use super::support::*;
use super::*;

const SKILLS: [&str; 10] = [
    "pdf",
    "graphify",
    "seo-audit",
    "blog-write",
    "ads-google",
    "slides",
    "humanizer",
    "market-copy",
    "stand-up",
    "brand",
];

async fn seed_catalog(actor: &SessionActor) {
    let skills = SKILLS
        .iter()
        .map(
            |name| distill_tools::implementations::skills::types::SkillInfo {
                name: name.to_string(),
                description: format!("Does {name} things."),
                path: format!("/skills/{name}/SKILL.md"),
                enabled: true,
                ..Default::default()
            },
        )
        .collect();
    actor
        .tool_bridge_handle()
        .seed_skill_discovery(None, None, skills, None, None, None, Default::default())
        .await;
}

/// A P6 answer that ranks `top` first with probability `p` and sets the
/// three gate nouls (acts, procedure, prose).
fn ranking(top: &str, p: f64, gates: [f64; 3]) -> distill_workspace::jev::JevAnswerSet {
    use distill_workspace::jev::Answer;
    let mut probabilities: std::collections::BTreeMap<String, f64> =
        SKILLS.iter().map(|name| (name.to_string(), 0.0)).collect();
    probabilities.insert(top.to_owned(), p);
    probabilities.insert("none".to_owned(), 1.0 - p);
    let answers = [
        (
            "best_skill".to_owned(),
            Answer::Choice {
                choice: top.to_owned(),
                probabilities,
                confidence: Some(p),
            },
        ),
        ("gate_acts".to_owned(), Answer::Noul { noul: gates[0] }),
        ("gate_procedure".to_owned(), Answer::Noul { noul: gates[1] }),
        ("gate_prose".to_owned(), Answer::Noul { noul: gates[2] }),
    ]
    .into_iter()
    .collect();
    distill_workspace::jev::JevAnswerSet {
        model: "test-decision-model".to_owned(),
        answers,
        usage: Default::default(),
        request_id: None,
        latency_ms: 1,
    }
}

async fn with_actor<F, Fut>(test: F)
where
    F: FnOnce(SessionActor) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            seed_catalog(&actor).await;
            test(actor).await;
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_flags();
        })
        .await;
}

/// The first prompt's prefix is built before the request enters the
/// conversation; the incoming text must still drive the selection, or the
/// full catalog reaches the model on every request of the session.
#[tokio::test(flavor = "current_thread")]
async fn first_prompt_projection_uses_the_incoming_request() {
    with_actor(|actor| async move {
        crate::jev::set_test_decision_answers([Some(ranking("pdf", 0.9, [0.9, 0.9, 0.1]))]);
        let projection = actor
            .jev_model_skill_projection(Some("export the quarterly report as a PDF"))
            .await
            .expect("a projection for a catalog larger than the descriptor limit");
        let envelope = projection.envelope;
        assert!(envelope.contains("/skills/pdf/SKILL.md"), "{envelope}");
        assert!(
            !envelope.contains("/skills/seo-audit/SKILL.md"),
            "{envelope}"
        );
        assert!(envelope.contains("<other_skills"), "{envelope}");
        assert!(
            envelope.contains("seo-audit"),
            "omitted skills stay listed by name"
        );
        assert!(envelope.contains("Full catalog index for recovery"));
    })
    .await;
}

/// The hint names the one skill the ranking trusts, with its path; a request
/// that needs no procedure gets an explicit "none"; a skill the user named is
/// pointed at without asking Jev.
#[tokio::test(flavor = "current_thread")]
async fn relevance_hint_names_the_skill_or_says_none_applies() {
    with_actor(|actor| async move {
        crate::jev::set_test_decision_answers([
            Some(ranking("slides", 0.85, [0.9, 0.9, 0.1])),
            Some(ranking("slides", 0.85, [0.1, 0.1, 0.9])),
        ]);
        let hint = actor
            .jev_skill_relevance_hint("build a deck for the board meeting")
            .await
            .expect("hint");
        assert!(hint.starts_with("<skill_relevance>\n"), "{hint}");
        assert!(
            hint.contains("`slides` (/skills/slides/SKILL.md)"),
            "{hint}"
        );
        let none = actor
            .jev_skill_relevance_hint("explain what a monad is")
            .await
            .expect("hint");
        assert!(
            none.contains("No skill in the catalog appears relevant"),
            "{none}"
        );
        let named = actor
            .jev_skill_relevance_hint("run /graphify on the docs folder")
            .await
            .expect("hint for a named skill");
        assert!(named.contains("The user named: `graphify`"), "{named}");
        assert_eq!(
            crate::jev::test_decision_answers_remaining(),
            0,
            "the named-skill hint must not spend a Jev call"
        );
    })
    .await;
}

fn tool_def(name: &str) -> ToolDefinition {
    ToolDefinition::function(name, None::<&str>, serde_json::json!({"type": "object"}))
}

fn family_answers(media: f64, schedule: f64) -> distill_workspace::jev::JevAnswerSet {
    use distill_workspace::jev::Answer;
    distill_workspace::jev::JevAnswerSet {
        model: "test-decision-model".to_owned(),
        answers: [
            ("1:family_media".to_owned(), Answer::Noul { noul: media }),
            (
                "1:family_schedule".to_owned(),
                Answer::Noul { noul: schedule },
            ),
        ]
        .into_iter()
        .collect(),
        usage: Default::default(),
        request_id: None,
        latency_ms: 1,
    }
}

/// Media and scheduling tools stay out of a coding request's tools array, are
/// not asked about again for the same request, and join for good once a
/// request needs them.
#[tokio::test(flavor = "current_thread")]
async fn optional_tool_families_join_when_needed_and_never_leave() {
    with_actor(|actor| async move {
        let defs = || {
            [
                "read_file",
                "run_terminal_command",
                "image_gen",
                "scheduler_list",
            ]
            .map(tool_def)
            .to_vec()
        };
        let kept = |defs: &[ToolDefinition]| -> Vec<String> {
            defs.iter().map(|d| d.function.name.clone()).collect()
        };
        actor.chat_state_handle.replace_conversation(vec![
            ConversationItem::system("sys"),
            ConversationItem::user("<user_query>\nfix the parser\n</user_query>"),
        ]);
        crate::jev::set_test_decision_answers([Some(family_answers(0.02, 0.01))]);
        let first = actor.jev_filter_tool_definitions(defs(), false).await;
        assert_eq!(kept(&first), ["read_file", "run_terminal_command"]);
        let again = actor.jev_filter_tool_definitions(defs(), false).await;
        assert_eq!(
            kept(&again),
            kept(&first),
            "same request: same tools, no new ask"
        );
        assert_eq!(crate::jev::test_decision_answers_remaining(), 0);

        actor.chat_state_handle.replace_conversation(vec![
            ConversationItem::system("sys"),
            ConversationItem::user("<user_query>\ndraw an icon for the app\n</user_query>"),
        ]);
        crate::jev::set_test_decision_answers([Some(family_answers(0.95, 0.01))]);
        let icon = actor.jev_filter_tool_definitions(defs(), false).await;
        assert!(
            kept(&icon).contains(&"image_gen".to_owned()),
            "{:?}",
            kept(&icon)
        );

        actor.chat_state_handle.replace_conversation(vec![
            ConversationItem::system("sys"),
            ConversationItem::user("<user_query>\nnow rename the module\n</user_query>"),
        ]);
        crate::jev::set_test_decision_answers([Some(family_answers(0.01, 0.01))]);
        let later = actor.jev_filter_tool_definitions(defs(), false).await;
        assert!(
            kept(&later).contains(&"image_gen".to_owned()),
            "an included family never leaves"
        );
        assert!(!kept(&later).contains(&"scheduler_list".to_owned()));
    })
    .await;
}

fn optional_family_defs() -> Vec<ToolDefinition> {
    ["read_file", "run_terminal_command", "image_gen", "scheduler_list"]
        .map(tool_def)
        .to_vec()
}

/// Turn-start answers: both optional families dropped, and B6 judging the
/// request multi-step with independent parts at `parallel`.
fn prelude_answers(parallel: f64) -> distill_workspace::jev::JevAnswerSet {
    use distill_workspace::jev::Answer;
    let mut answers = family_answers(0.02, 0.01);
    answers
        .answers
        .insert("2:fits_single_call".to_owned(), Answer::Noul { noul: 0.1 });
    answers
        .answers
        .insert("2:needs_parallel".to_owned(), Answer::Noul { noul: parallel });
    answers
}

/// Gives the session a worker model (or none), as a primary session gets one.
fn set_worker_model(actor: &SessionActor, worker: Option<&str>) {
    let agent = {
        let base = actor.agent.borrow();
        let mut context = base.prompt_context().clone();
        context.worker_model = worker.map(str::to_owned);
        distill_agent::Agent::new(
            base.definition().clone(),
            context,
            base.system_prompt().to_owned(),
            base.tool_bridge().clone(),
            base.reminder_policy().clone(),
            base.compaction_policy().clone(),
            base.hosted_tools().to_vec(),
            base.backend_search_enabled(),
        )
    };
    *actor.agent.borrow_mut() = agent;
}

fn ask_about(actor: &SessionActor, request: &str) {
    actor.chat_state_handle.replace_conversation(vec![
        ConversationItem::system("sys"),
        ConversationItem::user(format!("<user_query>\n{request}\n</user_query>")),
    ]);
}

fn asks_delegation(ids: &[String]) -> bool {
    ids.iter().any(|id| id == "2:fits_single_call") && ids.iter().any(|id| id == "2:needs_parallel")
}

fn b6_decisions(session_id: &str) -> Vec<String> {
    crate::jev::recorded_decisions_for_test(session_id)
        .into_iter()
        .filter(|(lever, _)| lever == "b6_delegation_hint")
        .map(|(_, decision)| decision)
        .collect()
}

async fn delegation_hints(actor: &SessionActor) -> Vec<String> {
    actor
        .chat_state_handle
        .get_conversation()
        .await
        .iter()
        .map(ConversationItem::text_content)
        .filter(|text| text.contains("<delegation_hint>"))
        .collect()
}

fn flags_with_b6(on: bool) -> distill_workspace::jev::JevFlags {
    distill_workspace::jev::JevFlags {
        b6_delegation_hint: on,
        ..distill_workspace::jev::JevFlags::harness_default()
    }
}

/// Shadow mode: with the flag off (the default) B6 still rides on the
/// turn-start request and its answer is recorded, but the prompt is unchanged.
#[tokio::test(flavor = "current_thread")]
async fn jev_b6_is_asked_and_recorded_without_a_hint_while_the_flag_is_off() {
    with_actor(|actor| async move {
        crate::jev::with_session_scope("b6-flag-off", async {
            crate::jev::set_test_flags(flags_with_b6(false));
            set_worker_model(&actor, Some("worker-model"));
            ask_about(&actor, "fix the parser and, separately, update the docs");
            crate::jev::set_test_decision_answers([Some(prelude_answers(0.9))]);
            actor
                .jev_filter_tool_definitions(optional_family_defs(), false)
                .await;
            let asked = crate::jev::take_test_asked_questions();
            assert_eq!(asked.len(), 1, "one turn-start request: {asked:?}");
            assert!(asks_delegation(&asked[0]), "{asked:?}");
            assert_eq!(b6_decisions("b6-flag-off"), ["delegate"]);
            actor.jev_flush_delegation_hint().await;
            assert!(delegation_hints(&actor).await.is_empty());
        })
        .await;
    })
    .await;
}

/// With the flag on, a suggestion reaches the turn once: later rounds and a
/// second pass over the same request add nothing.
#[tokio::test(flavor = "current_thread")]
async fn jev_b6_suggestion_adds_the_hint_exactly_once_with_the_flag_on() {
    with_actor(|actor| async move {
        crate::jev::with_session_scope("b6-flag-on", async {
            crate::jev::set_test_flags(flags_with_b6(true));
            set_worker_model(&actor, Some("worker-model"));
            ask_about(&actor, "fix the parser and, separately, update the docs");
            crate::jev::set_test_decision_answers([Some(prelude_answers(0.9))]);
            actor
                .jev_filter_tool_definitions(optional_family_defs(), false)
                .await;
            actor.jev_flush_delegation_hint().await;
            actor.jev_flush_delegation_hint().await;
            actor
                .jev_filter_tool_definitions(optional_family_defs(), false)
                .await;
            actor.jev_flush_delegation_hint().await;
            assert_eq!(
                delegation_hints(&actor).await,
                ["<delegation_hint>This request has independent parts: split them into narrow subagent assignments and launch them together in one message.</delegation_hint>"]
            );
            assert_eq!(b6_decisions("b6-flag-on"), ["delegate"]);
        })
        .await;
    })
    .await;
}

/// Delegation needs a worker to delegate to: without one the same pass sends
/// no B6 question, even with the flag on.
#[tokio::test(flavor = "current_thread")]
async fn jev_b6_without_a_worker_model_sends_no_delegation_questions() {
    with_actor(|actor| async move {
        crate::jev::with_session_scope("b6-no-worker", async {
            crate::jev::set_test_flags(flags_with_b6(true));
            set_worker_model(&actor, Some("worker-model"));
            ask_about(&actor, "fix the parser and, separately, update the docs");
            crate::jev::set_test_decision_answers([Some(prelude_answers(0.9))]);
            actor
                .jev_filter_tool_definitions(optional_family_defs(), false)
                .await;
            set_worker_model(&actor, None);
            ask_about(&actor, "now rename the module and update its tests");
            crate::jev::set_test_decision_answers([Some(prelude_answers(0.9))]);
            actor
                .jev_filter_tool_definitions(optional_family_defs(), false)
                .await;
            let asked = crate::jev::take_test_asked_questions();
            assert_eq!(asked.len(), 2, "{asked:?}");
            assert!(asks_delegation(&asked[0]), "with a worker: {asked:?}");
            assert!(
                !asked[1].iter().any(|id| id.starts_with("2:")),
                "without a worker: {asked:?}"
            );
            assert_eq!(
                b6_decisions("b6-no-worker"),
                ["delegate"],
                "only the request with a worker"
            );
        })
        .await;
    })
    .await;
}

async fn budget_reminders(actor: &SessionActor) -> usize {
    actor
        .chat_state_handle
        .get_conversation()
        .await
        .iter()
        .filter(|item| item.text_content().contains("Budget reached: stop exploring"))
        .count()
}

/// Fresh child: one reminder per turn, by request count or by prompt size.
#[tokio::test(flavor = "current_thread")]
async fn report_budget_reminds_a_long_child_once() {
    for (requests, tokens) in [(30, None), (3, Some(100_001))] {
        with_actor(|mut actor| async move {
            actor.startup_hints.report_budget = true;
            let mut sent = false;
            actor.flush_report_budget(2, Some(99_999), &mut sent).await;
            assert_eq!(budget_reminders(&actor).await, 0);
            actor.flush_report_budget(requests, tokens, &mut sent).await;
            actor.flush_report_budget(requests + 1, tokens, &mut sent).await;
            assert_eq!(budget_reminders(&actor).await, 1);
        })
        .await;
    }
}

/// Main sessions and forks never get `report_budget`, so they never see it.
#[tokio::test(flavor = "current_thread")]
async fn report_budget_skips_sessions_that_are_not_fresh_children() {
    with_actor(|actor| async move {
        let mut sent = false;
        actor.flush_report_budget(99, Some(500_000), &mut sent).await;
        assert_eq!(budget_reminders(&actor).await, 0);
    })
    .await;
}
