// Modified for Distill by Samuel Fajreldines, 2026.
//! Ultracode: the per-turn reminder is pushed only while the flag is on, and effort is untouched.
use super::support::*;
use super::*;

#[tokio::test]
async fn execution_mode_reminder_supersedes_previous_modes_without_changing_effort() {
    use distill_tool_types::ExecutionMode;
    tokio::task::LocalSet::new().run_until(async {
        let (actor, _gateway_rx) = build_actor().await;
        let before = actor.chat_state_handle.get_sampling_config().await.map(|cfg| cfg.reasoning_effort);
        for (mode, expected) in [
            (ExecutionMode::Normal, "Normal execution mode is active"),
            (ExecutionMode::Ultracode, "UltraCode mode is active"),
            (ExecutionMode::Ultrafast, "UltraFast mode is active"),
            (ExecutionMode::Ultrasave, "UltraSave mode is active"),
            (ExecutionMode::Normal, "Normal execution mode is active"),
        ] {
            actor.execution_mode.store(mode);
            actor.inject_execution_mode_reminder();
            let conversation = actor.chat_state_handle.get_conversation().await;
            let text = conversation.last().unwrap().text_content();
            assert!(text.contains(expected), "{text}");
            assert!(text.contains("supersedes all earlier"), "{text}");
        }
        assert_eq!(before, actor.chat_state_handle.get_sampling_config().await.map(|cfg| cfg.reasoning_effort));
    }).await;
}

/// Restating an unchanged mode at every turn start only adds an identical
/// message the model already has. The turn-start copy is skipped while the
/// latest directive in live history, after the last compaction output, is
/// the current mode's; a mode change or a compaction brings it back.
#[tokio::test(flavor = "current_thread")]
async fn turn_start_states_the_mode_once_until_it_changes_or_history_is_compacted() {
    use distill_tool_types::ExecutionMode;
    tokio::task::LocalSet::new()
        .run_until(async {
            let actor = super::support::plain_actor().await;
            let directives = |conversation: &[ConversationItem]| -> Vec<String> {
                conversation
                    .iter()
                    .map(ConversationItem::text_content)
                    .filter(|text| text.contains("supersedes all earlier execution mode reminders"))
                    .collect()
            };
            let turn = |request: &'static str| {
                let actor = &actor;
                async move {
                    actor.inject_execution_mode_reminder_at_turn_start().await;
                    actor
                        .chat_state_handle
                        .push_user_message(ConversationItem::user(request));
                    actor
                        .chat_state_handle
                        .push_assistant_response(ConversationItem::assistant("Done."));
                }
            };
            actor.execution_mode.store(ExecutionMode::Ultrasave);
            for request in ["one", "two", "three"] {
                turn(request).await;
            }
            let conversation = actor.chat_state_handle.get_conversation().await;
            assert_eq!(directives(&conversation).len(), 1, "{conversation:?}");

            actor.execution_mode.store(ExecutionMode::Normal);
            turn("four").await;
            let stated = directives(&actor.chat_state_handle.get_conversation().await);
            assert_eq!(stated.len(), 2);
            assert!(stated.last().is_some_and(|text| text.contains("Normal execution mode is active")));

            // Back to UltraSave: its old copy is superseded by the Normal one.
            actor.execution_mode.store(ExecutionMode::Ultrasave);
            turn("five").await;
            assert_eq!(directives(&actor.chat_state_handle.get_conversation().await).len(), 3);

            // A compaction output follows the copy: the next turn states the mode again.
            actor
                .chat_state_handle
                .push_user_message(ConversationItem::user_meta("summary of the work so far"));
            turn("six").await;
            let conversation = actor.chat_state_handle.get_conversation().await;
            assert_eq!(directives(&conversation).len(), 4);
            let summary = conversation
                .iter()
                .position(|item| item.text_content() == "summary of the work so far")
                .expect("summary");
            assert_eq!(
                directives(conversation.get(summary..).unwrap_or_default()).len(),
                1,
                "{conversation:?}"
            );

            // The mid-turn path is unchanged: it always states the mode.
            actor.inject_execution_mode_reminder();
            assert_eq!(directives(&actor.chat_state_handle.get_conversation().await).len(), 5);
        })
        .await;
}

/// A child that cannot spawn pays for delegation advice on every turn and may
/// try a spawn that is refused, so its directive drops exactly those sentences
/// and keeps the care and verification ones.
#[test]
fn leaf_ultracode_directive_drops_only_the_delegation_sentences() {
    use super::ultracode::{
        ULTRACODE_DELEGATION_SENTENCES, ULTRACODE_DIRECTIVE, ultracode_leaf_directive,
    };
    for sentence in ULTRACODE_DELEGATION_SENTENCES {
        assert!(
            ULTRACODE_DIRECTIVE.contains(sentence),
            "drifted: {sentence}"
        );
    }
    let leaf = ultracode_leaf_directive();
    assert!(!leaf.to_lowercase().contains("delegat"), "{leaf}");
    assert!(!leaf.contains("child"), "{leaf}");
    for kept in [
        "Ultracode mode is ON.",
        "Understand invariants and the root cause before editing; verify the changed behavior and relevant edge cases.",
        "Preserve explicit model/effort pins and capability and permission ceilings.",
        "Report unresolved uncertainty and concrete runtime limits.",
    ] {
        assert!(leaf.contains(kept), "{leaf}");
    }
    assert!(!leaf.contains("  "), "{leaf}");
}

#[tokio::test(flavor = "current_thread")]
async fn ultracode_child_gets_delegation_sentences_only_below_the_depth_ceiling() {
    use distill_tools::implementations::distill;
    use distill_tools::implementations::distill::task::types::ExecutionPolicy;
    use distill_tools::registry::types::ToolConfig;
    // Delegation sentences exist only in the today-row text.
    super::mode_policy::set_v2_modes_for_tests(Some(distill_tool_types::V2Modes::default()));
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut actor = super::support::plain_actor().await;
            *actor.agent.borrow_mut() = super::support::test_agent_with_tools(vec![
                ToolConfig::for_tool::<distill::TaskTool>(),
                ToolConfig::for_tool::<distill::KillTaskTool>(),
                ToolConfig::for_tool::<distill::TaskOutputTool>(),
            ])
            .await;
            actor.startup_hints.is_subagent = true;
            actor.tool_context.subagent_depth = 1;
            for (max_depth, can_delegate) in [(1, false), (2, true)] {
                let mode = std::sync::Arc::new(distill_tool_types::AtomicExecutionMode::new(
                    distill_tool_types::ExecutionMode::Ultracode,
                ));
                actor.execution_mode = mode.clone();
                actor.tool_context.execution_policy = Some(ExecutionPolicy {
                    mode,
                    max_depth,
                    off_max_depth: 1,
                    capability_ceiling: None,
                    allowed_subagent_types: None,
                });
                actor.inject_execution_mode_reminder();
                let conversation = actor.chat_state_handle.get_conversation().await;
                let text = conversation.last().unwrap().text_content();
                assert!(text.contains("Ultracode mode is ON"), "{text}");
                assert_eq!(
                    text.contains("Delegate only"),
                    can_delegate,
                    "max_depth {max_depth}: {text}"
                );
            }
        })
        .await;
    super::mode_policy::set_v2_modes_for_tests(None);
}

/// A directive that promises what the harness does not do, or asks for
/// delegation, steers the model against its mode's row; a long one costs
/// tokens on every statement. The v2 texts say only what their rows enforce
/// or what the model must do itself, share no sentence, and are shorter
/// together than the texts they replace.
#[test]
fn v2_directives_are_short_distinct_and_free_of_delegation_advice() {
    use super::ultracode::{
        ULTRACODE_DIRECTIVE, ULTRACODE_V2_DIRECTIVE, ULTRACODE_V2_REVIEW_SENTENCE,
        ULTRAFAST_DIRECTIVE, ULTRAFAST_V2_DIRECTIVE, ULTRASAVE_DIRECTIVE, ULTRASAVE_V2_DIRECTIVE,
    };
    let v2 = [
        ULTRAFAST_V2_DIRECTIVE,
        ULTRASAVE_V2_DIRECTIVE,
        ULTRACODE_V2_DIRECTIVE,
    ];
    let today = [
        ULTRAFAST_DIRECTIVE,
        ULTRASAVE_DIRECTIVE,
        ULTRACODE_DIRECTIVE,
    ];
    let length = |texts: &[&str]| texts.iter().map(|text| text.len()).sum::<usize>();
    assert!(
        length(&v2) < length(&today),
        "{} >= {}",
        length(&v2),
        length(&today)
    );
    for text in v2 {
        let lower = text.to_lowercase();
        for word in ["delegat", "subagent"] {
            assert!(!lower.contains(word), "{word}: {text}");
        }
    }
    let sentences = |text: &'static str| text.split_inclusive(". ").map(str::trim);
    for (index, text) in v2.into_iter().enumerate() {
        for other in v2.into_iter().skip(index + 1) {
            for sentence in sentences(text) {
                assert!(!other.contains(sentence), "shared: {sentence}");
            }
        }
    }
    assert!(ULTRACODE_V2_DIRECTIVE.ends_with(ULTRACODE_V2_REVIEW_SENTENCE));
}

/// With `[modes] v2 = []` every mode states exactly the text it stated
/// before the v2 texts existed; on its v2 row it states the v2 text, and an
/// UltraCode subagent, whose turns get no turn-end review, is not promised one.
#[tokio::test(flavor = "current_thread")]
async fn each_mode_states_its_v2_text_on_its_v2_row_and_its_old_text_with_v2_off() {
    use super::ultracode::{
        ULTRACODE_V2_DIRECTIVE, ULTRACODE_V2_REVIEW_SENTENCE, ULTRAFAST_DIRECTIVE,
        ULTRAFAST_V2_DIRECTIVE, ULTRASAVE_DIRECTIVE, ULTRASAVE_V2_DIRECTIVE,
        ultracode_leaf_directive,
    };
    use distill_tool_types::{ExecutionMode, V2Modes};
    // The texts the previous release sent, by length, so an edit to them shows.
    assert_eq!(
        [
            super::ultracode::ULTRACODE_DIRECTIVE.len(),
            ULTRAFAST_DIRECTIVE.len(),
            ULTRASAVE_DIRECTIVE.len(),
        ],
        [1537, 1118, 1087]
    );
    let normal = "Normal execution mode is active. Follow the configured default execution policy.";
    let all = V2Modes::default()
        .with(ExecutionMode::Ultrafast)
        .with(ExecutionMode::Ultrasave)
        .with(ExecutionMode::Ultracode);
    let child_ultracode =
        ULTRACODE_V2_DIRECTIVE.replace(&format!(" {ULTRACODE_V2_REVIEW_SENTENCE}"), "");
    // The test actor has no spawn tool, so the today-row UltraCode text is the leaf one.
    let leaf = ultracode_leaf_directive();
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut actor = super::support::plain_actor().await;
            for (v2, is_subagent, mode, expected) in [
                (V2Modes::default(), false, ExecutionMode::Normal, normal),
                (V2Modes::default(), false, ExecutionMode::Ultrafast, ULTRAFAST_DIRECTIVE),
                (V2Modes::default(), false, ExecutionMode::Ultrasave, ULTRASAVE_DIRECTIVE),
                (V2Modes::default(), false, ExecutionMode::Ultracode, leaf.as_str()),
                (all, false, ExecutionMode::Normal, normal),
                (all, false, ExecutionMode::Ultrafast, ULTRAFAST_V2_DIRECTIVE),
                (all, false, ExecutionMode::Ultrasave, ULTRASAVE_V2_DIRECTIVE),
                (all, false, ExecutionMode::Ultracode, ULTRACODE_V2_DIRECTIVE),
                (all, true, ExecutionMode::Ultracode, child_ultracode.as_str()),
            ] {
                super::mode_policy::set_v2_modes_for_tests(Some(v2));
                actor.startup_hints.is_subagent = is_subagent;
                actor.execution_mode.store(mode);
                actor.inject_execution_mode_reminder();
                let conversation = actor.chat_state_handle.get_conversation().await;
                let expected = super::reminders::wrap_in_reminder_tag(
                    &format!(
                        "Current execution mode supersedes all earlier execution mode reminders. {expected}"
                    ),
                    actor.reminder_wrapper_tag(),
                );
                assert_eq!(
                    conversation.last().unwrap().text_content(),
                    expected.text_content(),
                    "{mode:?} subagent={is_subagent}"
                );
            }
            assert!(!child_ultracode.contains("review"), "{child_ultracode}");
        })
        .await;
    super::mode_policy::set_v2_modes_for_tests(None);
}

/// UltraFast and UltraSave send only direct-work tools, so planning, feedback,
/// scheduling and orchestration schemas stop costing tokens and whole rounds.
#[test]
fn execution_modes_prune_tool_definitions_to_direct_work() {
    use crate::sampling::types::ToolDefinition;
    use distill_tool_types::ExecutionMode;
    let offered = [
        "ask_stored_output",
        "browser__screenshot",
        "ask_user_question",
        "enter_plan_mode",
        "get_command_or_subagent_output",
        "grep",
        "jev_decide",
        "list_dir",
        "monitor",
        "read_file",
        "run_terminal_command",
        "scheduler_create",
        "search_replace",
        "search_tool",
        "send_feedback",
        "spawn_subagent",
        "todo_write",
        "use_tool",
        "workflow",
        "write",
    ];
    let defs: Vec<ToolDefinition> = offered
        .iter()
        .map(|name| ToolDefinition::function(*name, None::<&str>, serde_json::json!({})))
        .collect();
    let names = |defs: &[ToolDefinition]| {
        defs.iter()
            .map(|def| def.function.name.clone())
            .collect::<Vec<_>>()
    };
    let all: Vec<String> = offered.iter().map(|name| name.to_string()).collect();
    for mode in [ExecutionMode::Normal, ExecutionMode::Ultracode] {
        assert_eq!(
            names(&super::execution_mode_tool_definitions(mode, defs.clone())),
            all
        );
    }
    for mode in [ExecutionMode::Ultrafast, ExecutionMode::Ultrasave] {
        let kept = names(&super::execution_mode_tool_definitions(mode, defs.clone()));
        for name in &kept {
            assert!(
                super::EXECUTION_MODE_TOOLS.contains(&name.as_str())
                    || name == "spawn_subagent" || name == "browser__screenshot",
                "{mode:?} kept {name}"
            );
        }
        // scheduler_create: /loop expands into a scheduler_create call in every mode.
        for required in ["read_file", "write", "search_replace", "run_terminal_command", "browser__screenshot", "scheduler_create"] {
            assert!(kept.iter().any(|name| name == required), "{mode:?}: {kept:?}");
        }
        // Both modes delegate: UltraFast to fan independent work out in parallel,
        // UltraSave to the cheaper worker model.
        assert!(
            kept.iter().any(|name| name == "spawn_subagent"),
            "{mode:?}: {kept:?}"
        );
        for dropped in ["todo_write", "workflow", "send_feedback"] {
            assert!(
                !kept.iter().any(|name| name == dropped),
                "{mode:?} kept {dropped}"
            );
        }
    }
}

/// Runs `steps` (effective modes Auto picks, and prompt sizes the last main
/// response reported) and returns the tools array each pick would send.
async fn auto_tool_arrays(
    actor: &SessionActor,
    steps: &[(distill_tool_types::ExecutionMode, Option<u64>)],
) -> Vec<Vec<String>> {
    let mut arrays = Vec::new();
    for (mode, prompt_tokens) in steps {
        if let Some(tokens) = prompt_tokens {
            let mut ledger = actor.jev_ledger.borrow_mut();
            ledger.note_round("main", None);
            ledger.add_usage(*tokens, 1);
        }
        actor.execution_mode.pick_auto(*mode);
        let defs = actor.prepare_tool_definitions().await;
        arrays.push(
            defs.iter()
                .map(|def| serde_json::to_string(def).expect("tool definition serializes"))
                .collect(),
        );
    }
    arrays
}

async fn actor_with_lean_and_full_tools() -> SessionActor {
    use distill_tools::implementations::distill;
    use distill_tools::registry::types::ToolConfig;
    let actor = super::support::plain_actor().await;
    *actor.agent.borrow_mut() = super::support::test_agent_with_tools(vec![
        ToolConfig::for_tool::<distill::ReadFileTool>(),
        ToolConfig::for_tool::<distill::GrepTool>(),
        ToolConfig::for_tool::<distill::todo::TodoWriteTool>(),
    ])
    .await;
    // Every Jev question in these tests gets no answer; nothing leaves the process.
    crate::jev::set_test_decision_answers([]);
    actor
}

/// Under Auto, a switch between the lean and the full tool list changed the
/// tools array and re-read the whole prompt uncached. With `b8_stable_tools`
/// (the default) the array only grows: once the full list went out, UltraSave and UltraFast
/// send exactly the same bytes, so the turn loop sees the same tools and keeps
/// the cached prompt. A small or compacted prompt may shrink it again, and a
/// session that started lean grows once, on its first move up.
#[tokio::test(flavor = "current_thread")]
async fn auto_never_shrinks_the_tools_array_once_the_full_list_went_out() {
    use distill_tool_types::ExecutionMode::{Normal, Ultracode, Ultrafast, Ultrasave};
    const LARGE: u64 = super::jev_ledger::EFFORT_SWITCH_FREE_PROMPT_TOKENS;
    tokio::task::LocalSet::new()
        .run_until(async {
            crate::jev::set_test_flags(distill_workspace::jev::JevFlags::harness_default());
            let actor = actor_with_lean_and_full_tools().await;
            actor.execution_mode.store_auto(Ultrasave);
            let arrays = auto_tool_arrays(
                &actor,
                &[
                    (Ultrasave, None),
                    (Ultrafast, Some(LARGE)),
                    (Normal, Some(LARGE)),
                    (Ultrasave, Some(LARGE)),
                    (Ultrafast, Some(LARGE)),
                    (Ultracode, Some(LARGE)),
                    (Ultrasave, Some(LARGE)),
                ],
            )
            .await;
            let [lean, lean_again, full, rest @ ..] = arrays.as_slice() else {
                unreachable!()
            };
            assert!(lean.len() < full.len(), "the session started on the lean list");
            assert_eq!(lean, lean_again, "a lean start stays lean until a move up");
            for (index, array) in rest.iter().enumerate() {
                assert_eq!(array, full, "step {index} after the move up changed the tools array");
            }

            // A compaction rewrote the prompt: the lean list may come back,
            // and the next move up grows the array once again.
            actor.jev_ledger.borrow_mut().note_compaction();
            let arrays =
                auto_tool_arrays(&actor, &[(Ultrasave, None), (Normal, None), (Ultrafast, Some(LARGE))])
                    .await;
            assert_eq!(arrays, [lean.clone(), full.clone(), full.clone()]);

            // Under 16k prompt tokens a miss is cheap, so the lean list is free to return.
            let arrays = auto_tool_arrays(&actor, &[(Ultrafast, Some(LARGE - 1))]).await;
            assert_eq!(arrays, std::slice::from_ref(lean));

            // An explicit /mode is the user's choice and re-projects at once.
            auto_tool_arrays(&actor, &[(Normal, Some(LARGE))]).await;
            actor.execution_mode.store(Ultrasave);
            let fixed = actor.prepare_tool_definitions().await;
            let fixed: Vec<String> = fixed
                .iter()
                .map(|def| serde_json::to_string(def).unwrap())
                .collect();
            assert_eq!(&fixed, lean);
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_flags();
        })
        .await;
}

/// With `[jev.ladder] b8_stable_tools = false` every Auto pick re-projects:
/// the lean modes always send the lean list.
#[tokio::test(flavor = "current_thread")]
async fn without_stable_tools_auto_reprojects_on_every_pick() {
    use distill_tool_types::ExecutionMode::{Normal, Ultrafast, Ultrasave};
    const LARGE: u64 = super::jev_ledger::EFFORT_SWITCH_FREE_PROMPT_TOKENS;
    tokio::task::LocalSet::new()
        .run_until(async {
            crate::jev::set_test_flags(distill_workspace::jev::JevFlags {
                b8_stable_tools: false,
                ..distill_workspace::jev::JevFlags::harness_default()
            });
            let actor = actor_with_lean_and_full_tools().await;
            actor.execution_mode.store_auto(Normal);
            let arrays = auto_tool_arrays(
                &actor,
                &[(Normal, None), (Ultrasave, Some(LARGE)), (Ultrafast, Some(LARGE)), (Normal, Some(LARGE))],
            )
            .await;
            assert_eq!(arrays[0], arrays[3]);
            assert_eq!(arrays[1], arrays[2], "the two lean modes send the same list");
            assert!(arrays[1].len() < arrays[0].len());
            assert!(!actor.jev_ledger.borrow().auto_full_tools_sent);
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_flags();
        })
        .await;
}

#[tokio::test]
async fn execution_policy_survives_agent_rebuild_and_shared_off() {
    use distill_tools::implementations::distill::task::types::ExecutionPolicy;
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut spec = crate::session::agent_rebuild::test_rebuild_spec_default();
            let enabled = std::sync::Arc::new(distill_tool_types::AtomicExecutionMode::new(distill_tool_types::ExecutionMode::Ultracode));
            let mutable = std::sync::Arc::get_mut(&mut spec).unwrap();
            mutable.execution_policy = Some(ExecutionPolicy {
                mode: enabled.clone(),
                max_depth: 2,
                off_max_depth: 1,
                capability_ceiling: None,
                allowed_subagent_types: None,
            });
            mutable.subagent_event_tx = Some(tokio::sync::mpsc::unbounded_channel().0);
            let first = spec
                .build_agent(
                    distill_agent::config::AgentDefinition::default_distill(),
                    distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                    crate::test_support::TEST_MODEL,
                )
                .await
                .unwrap();
            let rebuilt = spec
                .build_agent(
                    distill_agent::config::AgentDefinition::default_distill(),
                    distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                    crate::test_support::TEST_MODEL,
                )
                .await
                .unwrap();
            let before = first
                .tool_bridge()
                .toolset()
                .get_resource_cloned::<ExecutionPolicy>()
                .await
                .unwrap();
            let after = rebuilt
                .tool_bridge()
                .toolset()
                .get_resource_cloned::<ExecutionPolicy>()
                .await
                .unwrap();
            assert!(std::sync::Arc::ptr_eq(&before.mode, &after.mode));
            assert_eq!(after.max_depth, 2);
            enabled.store(distill_tool_types::ExecutionMode::Normal);
            assert!(!before.is_enabled() && !after.is_enabled());
        })
        .await;
}
