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
            (ExecutionMode::Ultracode, "Ultracode mode is ON"),
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
