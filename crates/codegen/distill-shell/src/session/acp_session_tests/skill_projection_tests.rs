// Modified for Distill by Samuel Fajreldines, 2026.
//! Actor-level tests for the request-aware skill listing (Jev P6) and the
//! per-turn `<skill_relevance>` hint.
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
