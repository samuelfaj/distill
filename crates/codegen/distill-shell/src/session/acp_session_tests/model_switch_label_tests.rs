// Modified for Distill by Samuel Fajreldines, 2026.
use distill_sampling_types::conversation::ConversationItem;

use super::reconciled_orchestration_head;
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
            let (mut actor, _gateway_rx) = build_actor().await;
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("spawn-time prompt"),
                ConversationItem::user("hi"),
            ]);
            std::sync::Arc::get_mut(&mut actor)
                .expect("unique test actor")
                .compaction
                .context_window_override = Some(std::num::NonZeroU64::new(100_000).unwrap());

            actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: distill_sampler::SamplerConfig {
                        model: "gpt-6-astra".to_owned(),
                        context_window: 272_000,
                        ..distill_sampler::SamplerConfig::default()
                    },
                    canonical_model_id: Some(agent_client_protocol::ModelId::new(
                        "chatgpt/gpt-6-astra",
                    )),
                    model_selection_intent: true,
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
            let signals = actor
                .signals_handle()
                .snapshot()
                .await
                .expect("signals actor should be alive");
            assert_eq!(signals.context_window_tokens, 100_000);
            assert_eq!(
                actor.build_status_context().await.context_window.context_window_size,
                Some(100_000)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effort_only_model_switch_does_not_create_child_model_pin() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (mut actor, _gateway_rx) = build_actor().await;
            let actor_mut = std::sync::Arc::get_mut(&mut actor).expect("unique test actor");
            actor_mut.startup_hints.is_subagent = true;
            actor_mut.startup_hints.explicit_model_override = false;
            actor.model_routing_locked.set(false);

            actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: distill_sampler::SamplerConfig {
                        model: "switch-target".to_owned(),
                        context_window: 256_000,
                        ..distill_sampler::SamplerConfig::default()
                    },
                    canonical_model_id: Some(agent_client_protocol::ModelId::new(
                        "same-catalog-model",
                    )),
                    model_selection_intent: false,
                    use_concise: false,
                    is_family_switch: false,
                    apply_prompt_override: false,
                    skip_prompt_rewrite: true,
                    auto_compact_threshold_percent: 85,
                    system_prompt_label: distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_owned(),
                })
                .await
                .expect("effort-only switch succeeds");

            assert!(
                !actor.model_routing_locked.get(),
                "an effort-only same-model request must not create a child model pin"
            );
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

/// The head the model reads must name the worker this session's children run on:
/// setting the session's worker adds `<orchestration>` (after a running turn ends),
/// and clearing it removes the section.
#[tokio::test(flavor = "current_thread")]
async fn session_worker_change_rerenders_the_orchestration_section() {
    use crate::session::handle::SessionWorker;
    tokio::task::LocalSet::new()
        .run_until(async {
            crate::jev::set_test_worker_model(None);
            let (mut actor, _gateway_rx) = build_actor().await;
            let mut spec = crate::session::agent_rebuild::test_rebuild_spec_default();
            let fields = std::sync::Arc::get_mut(&mut spec).expect("unique test spec");
            fields.subagents_enabled = true;
            fields.models_manager.insert_test_entry(
                "worker-x",
                crate::agent::config::ModelEntry::fallback(
                    "worker-x",
                    &crate::agent::config::EndpointsConfig::default(),
                ),
            );
            let agent = spec
                .build_agent(
                    distill_agent::AgentDefinition::default_distill(),
                    distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                    crate::test_support::TEST_MODEL,
                )
                .await
                .expect("agent build succeeds");
            let spawn_head = agent.system_prompt().to_owned();
            assert!(!spawn_head.contains("<orchestration>"), "no worker yet");
            *actor.agent.borrow_mut() = agent;
            std::sync::Arc::get_mut(&mut actor)
                .expect("unique test actor")
                .rebuild_spec = std::sync::Arc::clone(&spec);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system(spawn_head),
                ConversationItem::user("hi"),
            ]);

            *spec.worker_override.write() = Some(SessionWorker {
                model_id: Some("worker-x".to_owned()),
                effort: None,
            });
            actor.state.lock().await.running_task = Some(running_task_stub("running"));
            actor.refresh_worker_prompt().await;
            let head = head_text(&actor.chat_state_handle.get_conversation().await);
            assert!(
                !head.contains("<orchestration>"),
                "a running turn keeps its prompt"
            );
            assert!(actor.worker_prompt_pending.get());
            if let Some(task) = actor.state.lock().await.running_task.take() {
                task.handle.abort();
            }
            actor.refresh_worker_prompt().await;
            let head = head_text(&actor.chat_state_handle.get_conversation().await);
            assert!(
                head.contains("<orchestration>") && head.contains("Worker model: `worker-x`"),
                "the head must name the session's worker, got: {head:.200}"
            );
            assert!(!actor.worker_prompt_pending.get());
            assert_eq!(actor.agent.borrow().system_prompt(), head);

            *spec.worker_override.write() = None;
            actor.refresh_worker_prompt().await;
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                !head_text(&conv).contains("<orchestration>"),
                "a cleared worker falls back to the config, which has none"
            );
            assert_eq!(2, conv.len(), "the refresh swaps only the head");
            crate::jev::clear_test_worker_model();
        })
        .await;
}

/// A spec whose agent may delegate, with `workers` in its catalog.
fn delegating_spec(
    workers: &[&str],
) -> std::sync::Arc<crate::session::agent_rebuild::AgentRebuildSpec> {
    let mut spec = crate::session::agent_rebuild::test_rebuild_spec_default();
    let fields = std::sync::Arc::get_mut(&mut spec).expect("unique test spec");
    fields.subagents_enabled = true;
    for worker in workers {
        fields.models_manager.insert_test_entry(
            *worker,
            crate::agent::config::ModelEntry::fallback(
                worker,
                &crate::agent::config::EndpointsConfig::default(),
            ),
        );
    }
    spec
}

/// The prompt `agent` renders when its children run on `worker`.
async fn render_naming(agent: &distill_agent::Agent, worker: Option<&str>) -> String {
    let mut context = agent.prompt_context().clone();
    context.worker_model = worker.map(str::to_owned);
    context
        .render(agent.tool_bridge())
        .await
        .expect("prompt renders")
}

/// The head reads and rewrites the section the real template renders, down to the byte.
#[tokio::test(flavor = "current_thread")]
async fn orchestration_head_reconciliation_matches_the_rendered_template() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let agent = delegating_spec(&[])
                .build_agent(
                    distill_agent::AgentDefinition::default_distill(),
                    distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                    crate::test_support::TEST_MODEL,
                )
                .await
                .expect("agent build succeeds");
            let none = render_naming(&agent, None).await;
            let a = render_naming(&agent, Some("worker-a")).await;
            let b = render_naming(&agent, Some("worker-b")).await;
            assert!(a.contains("Worker model: `worker-a`"), "{a:.300}");
            assert!(!none.contains("<orchestration>"));

            assert_eq!(reconciled_orchestration_head(&a, &b), Some(b.clone()));
            assert_eq!(reconciled_orchestration_head(&none, &b), Some(b.clone()));
            assert_eq!(reconciled_orchestration_head(&b, &none), Some(none));
            assert_eq!(reconciled_orchestration_head(&b, &b), None);
        })
        .await;
}

/// A resumed head keeps the text it was saved with, so it can name another worker than the one
/// this session's children run on while the agent's own prompt is already right.
#[tokio::test(flavor = "current_thread")]
async fn refresh_rewrites_a_stale_head_and_leaves_a_matching_one_alone() {
    use crate::session::handle::SessionWorker;
    tokio::task::LocalSet::new()
        .run_until(async {
            let (mut actor, _gateway_rx) = build_actor().await;
            let spec = delegating_spec(&["worker-a", "worker-b"]);
            *spec.worker_override.write() = Some(SessionWorker {
                model_id: Some("worker-b".to_owned()),
                effort: None,
            });
            let agent = spec
                .build_agent(
                    distill_agent::AgentDefinition::default_distill(),
                    distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                    crate::test_support::TEST_MODEL,
                )
                .await
                .expect("agent build succeeds");
            assert!(
                agent.system_prompt().contains("Worker model: `worker-b`"),
                "the agent already names the session's worker"
            );
            let saved = format!(
                "{}\n\n<human_rules>\nbe terse\n</human_rules>",
                render_naming(&agent, Some("worker-a")).await
            );
            *actor.agent.borrow_mut() = agent;
            std::sync::Arc::get_mut(&mut actor)
                .expect("unique test actor")
                .rebuild_spec = std::sync::Arc::clone(&spec);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system(saved.clone()),
                ConversationItem::user("hi"),
            ]);

            actor.state.lock().await.running_task = Some(running_task_stub("running"));
            actor.refresh_worker_prompt().await;
            assert_eq!(
                head_text(&actor.chat_state_handle.get_conversation().await),
                saved,
                "a running turn keeps its head"
            );
            assert!(actor.worker_prompt_pending.get());
            if let Some(task) = actor.state.lock().await.running_task.take() {
                task.handle.abort();
            }

            actor.refresh_worker_prompt().await;
            let conv = actor.chat_state_handle.get_conversation().await;
            let head = head_text(&conv);
            assert!(
                head.contains("Worker model: `worker-b`") && !head.contains("`worker-a`"),
                "the head must name the session's worker, got: {head:.300}"
            );
            assert!(head.ends_with("</human_rules>"), "only the section changes");
            assert!(!actor.worker_prompt_pending.get());
            assert_eq!(2, conv.len(), "the refresh swaps only the head");

            actor.compaction.prefire.store(AsyncCompactionCache {
                note1: "NOTE1".to_owned(),
                prefix_len: 1,
                fingerprint: 42,
                model_slug: "switch-target".to_owned(),
                pass1_latency_ms: 5,
            });
            actor.refresh_worker_prompt().await;
            assert_eq!(
                head_text(&actor.chat_state_handle.get_conversation().await),
                head,
                "a head that already agrees is left as saved"
            );
            assert!(
                actor.compaction.prefire.has_cache(),
                "an untouched head keeps the prefire cache built on it"
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
