// Modified for Distill by Samuel Fajreldines, 2026.
//! Ultracode: the per-turn reminder is pushed only while the flag is on, and effort is untouched.
use super::support::*;
use super::*;

#[tokio::test]
async fn ultracode_reminder_only_when_on_and_effort_untouched() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            let effort_before = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|cfg| cfg.reasoning_effort);

            actor.inject_ultracode_reminder();
            assert_eq!(
                actor.chat_state_handle.get_conversation_len().await,
                0,
                "off must push nothing"
            );

            actor.ultracode.store(true, std::sync::atomic::Ordering::Relaxed);
            actor.inject_ultracode_reminder();
            let conv = actor.chat_state_handle.get_conversation().await;
            assert_eq!(conv.len(), 1);
            let text = conv[0].text_content();
            assert!(text.contains("<system-reminder>"), "{text}");
            assert!(text.contains("Ultracode mode is ON"), "{text}");

            actor.ultracode.store(false, std::sync::atomic::Ordering::Relaxed);
            actor.inject_ultracode_reminder();
            assert_eq!(
                actor.chat_state_handle.get_conversation_len().await,
                1,
                "off again must push nothing more"
            );

            let effort_after = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|cfg| cfg.reasoning_effort);
            assert_eq!(effort_before, effort_after, "Ultracode must not touch effort");
        })
        .await;
}

#[tokio::test]
async fn ultracode_policy_survives_agent_rebuild_and_shared_off() {
    use distill_tools::implementations::distill::task::types::UltracodePolicy;
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut spec = crate::session::agent_rebuild::test_rebuild_spec_default();
            let enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let mutable = std::sync::Arc::get_mut(&mut spec).unwrap();
            mutable.ultracode_policy = Some(UltracodePolicy {
                enabled: enabled.clone(),
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
                .get_resource_cloned::<UltracodePolicy>()
                .await
                .unwrap();
            let after = rebuilt
                .tool_bridge()
                .toolset()
                .get_resource_cloned::<UltracodePolicy>()
                .await
                .unwrap();
            assert!(std::sync::Arc::ptr_eq(&before.enabled, &after.enabled));
            assert_eq!(after.max_depth, 2);
            enabled.store(false, std::sync::atomic::Ordering::Relaxed);
            assert!(!before.is_enabled() && !after.is_enabled());
        })
        .await;
}
