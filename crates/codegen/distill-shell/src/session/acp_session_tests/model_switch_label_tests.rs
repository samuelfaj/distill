// Modified for Distill by Samuel Fajreldines, 2026.
use distill_sampling_types::conversation::ConversationItem;

use super::support::{build_actor, running_task_stub};
use crate::session::compaction_config::AsyncCompactionCache;

const SWITCH_TARGET_LABEL: &str = "Aurora";

fn head_text(conv: &[ConversationItem]) -> String {
    let Some(ConversationItem::System(sys)) = conv.first() else {
        panic!(
            "conversation must start with a System item, got {:?}",
            conv.first()
        );
    };
    sys.content.as_ref().to_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_relabels_live_agent_and_system_head() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("spawn-time prompt"),
                ConversationItem::user("hi"),
            ]);

            actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: distill_sampler::SamplerConfig {
                        model: "switch-target".to_owned(),
                        context_window: 256_000,
                        ..distill_sampler::SamplerConfig::default()
                    },
                    canonical_model_id: None,
                    use_concise: false,
                    is_family_switch: false,
                    apply_prompt_override: true,
                    skip_prompt_rewrite: false,
                    auto_compact_threshold_percent: 85,
                    system_prompt_label: SWITCH_TARGET_LABEL.to_owned(),
                })
                .await
                .expect("model switch succeeds");

            let conv = actor.chat_state_handle.get_conversation().await;
            let head = head_text(&conv);
            assert!(
                head.contains(SWITCH_TARGET_LABEL),
                "system head must render the new model's label, got: {head:.120}"
            );
            let agent = actor.agent.borrow();
            assert_eq!(
                SWITCH_TARGET_LABEL,
                agent.prompt_context().system_prompt_label
            );
            assert_eq!(agent.system_prompt(), head);
            assert_eq!(2, conv.len(), "the switch swaps only the head");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn relabel_skipped_mid_turn_keeps_prefire_cache() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.compaction.prefire.store(AsyncCompactionCache {
                note1: "NOTE1".to_owned(),
                prefix_len: 1,
                fingerprint: 42,
                model_slug: "switch-target".to_owned(),
                pass1_latency_ms: 5,
            });
            actor.state.lock().await.running_task = Some(running_task_stub("running"));

            actor
                .relabel_agent_system_prompt(SWITCH_TARGET_LABEL.to_owned())
                .await;

            assert!(
                actor.compaction.prefire.has_cache(),
                "a skipped relabel must not drop a prefire built on the still-live prompt"
            );
            assert_eq!(
                distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                actor.agent.borrow().prompt_context().system_prompt_label
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn zero_turn_rebuild_renders_switch_target_label() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;

            actor
                .handle_rebuild_agent_for_definition(
                    distill_agent::AgentDefinition::default_distill(),
                    SWITCH_TARGET_LABEL.to_owned(),
                )
                .await
                .expect("zero-turn rebuild succeeds");

            let conv = actor.chat_state_handle.get_conversation().await;
            let head = head_text(&conv);
            assert!(
                head.contains(SWITCH_TARGET_LABEL),
                "rebuilt head must render the new model's label, got: {head:.120}"
            );
            let agent = actor.agent.borrow();
            assert_eq!(
                SWITCH_TARGET_LABEL,
                agent.prompt_context().system_prompt_label
            );
            assert_eq!(agent.system_prompt(), head);
        })
        .await;
}
