// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for the "MCP servers currently connecting" reminder rendering (`format_mcp_connecting_reminder`).
//!
//! The delivery-tool wording exists because some headless clients deliver output ONLY through MCP tools.
//! Telling the model to "proceed without" a still-connecting server made it answer in plain text that no user ever saw.
//! The wording is gated on the explicit `startupHints.deliveryTools` opt-in, NOT on `nonInteractive`.
//! Defaults therefore stay unchanged for every client that does not declare delivery tools.
//! SDK/stdio consumers read plain-text responses; subagents report to their parent.

use agent_client_protocol as acp;
use distill_mcp::servers::McpInitStrategy;

use super::mcp::format_mcp_connecting_reminder;
use super::support::*;
use super::{ConversationItem, PromptMode, SessionActor};

#[test]
fn default_reminder_lists_connecting_servers() {
    let text = format_mcp_connecting_reminder(&["alpha".to_string()], &[]);
    assert!(text.contains("- alpha\n"));
    assert!(!text.contains("alpha__post"));
    assert!(!text.contains("alpha__ask"));
}

#[test]
fn declared_delivery_tools_are_named_in_the_reminder() {
    let text = format_mcp_connecting_reminder(
        &["alpha".to_string(), "beta".to_string()],
        &["alpha__post".to_string(), "alpha__ask".to_string()],
    );
    assert!(text.contains("- alpha\n- beta\n"));
    assert!(text.contains("alpha__post, alpha__ask"));
}

/// A resident `session/load` carrying explicit `startupHints` re-applies the attaching client's policy.
/// The `UpdateAttachPolicy` message is handled by `apply_attach_policy`.
/// The MCP init strategy and delivery tools must track the CURRENT attachment, not the client that originally spawned the actor.
#[tokio::test(flavor = "current_thread")]
async fn apply_attach_policy_tracks_the_current_attachment() {
    // `create_test_actor` spawns local tasks; it must run inside a LocalSet.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 100_000, 85, gateway_tx, persistence_tx).await;

            // Strategy assertions only hold without the env override (the override deliberately wins over hints; tests must not mutate global env)
            let env_override =
                std::env::var("MCP_INIT_STRATEGY").is_ok_and(|v| !v.trim().is_empty());

            // Interactive attachment with no delivery tools.
            actor.apply_attach_policy(&crate::session::StartupHints::default());
            if !env_override {
                assert_eq!(actor.mcp_strategy.get(), McpInitStrategy::Progressive);
            }
            assert!(actor.delivery_tools.borrow().is_empty());
            assert!(!actor.attach_non_interactive.get());

            // Headless attachment re-applies Blocking and its delivery tools, and the OAuth-interactivity flag follows the attachment
            // A headless re-attach must not run interactive browser OAuth on the MCP re-init
            actor.apply_attach_policy(&crate::session::StartupHints {
                non_interactive: true,
                delivery_tools: vec!["srv__post".to_string()],
                ..Default::default()
            });
            if !env_override {
                assert_eq!(actor.mcp_strategy.get(), McpInitStrategy::Blocking);
            }
            assert_eq!(
                *actor.delivery_tools.borrow(),
                vec!["srv__post".to_string()]
            );
            assert!(actor.attach_non_interactive.get());
        })
        .await;
}

/// A policy-changing re-attach must re-arm the once-per-actor connecting reminder.
/// A latched default reminder must not suppress the delivery wording for a later delivery-tool attachment.
/// An identical re-attach keeps the latch so per-prompt loads don't re-inject each turn.
#[tokio::test(flavor = "current_thread")]
async fn apply_attach_policy_rearms_connecting_reminder_only_on_change() {
    // `create_test_actor` spawns local tasks; it must run inside a LocalSet.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 100_000, 85, gateway_tx, persistence_tx).await;

            let headless = crate::session::StartupHints {
                non_interactive: true,
                delivery_tools: vec!["srv__post".to_string()],
                ..Default::default()
            };

            // Simulate a reminder already injected for the spawning client.
            actor.mcp_connecting_reminder_injected.set(true);
            actor.apply_attach_policy(&headless);
            assert!(
                !actor.mcp_connecting_reminder_injected.get(),
                "policy change must re-arm the latched reminder"
            );

            // Same policy again: the latch (re-set after an injection) must hold.
            actor.mcp_connecting_reminder_injected.set(true);
            actor.apply_attach_policy(&headless);
            assert!(
                actor.mcp_connecting_reminder_injected.get(),
                "identical re-attach must not re-arm the reminder"
            );
        })
        .await;
}

/// Turn futures overflow the default test stack; run them on 16 MiB like `turn/disk_full_tests.rs`.
fn block_on_session_turn<F, Fut>(f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            tokio::task::LocalSet::new().block_on(&rt, f());
        })
        .expect("spawn large-stack test thread")
        .join()
        .expect("test thread");
}

fn stdio_server(name: &str) -> acp::McpServer {
    acp::McpServer::Stdio(
        acp::McpServerStdio::new(name.to_string(), "true")
            .args(vec![])
            .env(vec![]),
    )
}

async fn mark_mid_handshake(actor: &SessionActor, server: &str) {
    let mut state = actor.mcp_state.lock().await;
    state.configs = vec![stdio_server(server)];
    std::mem::forget(state.try_start_init().expect("fixture must enter Starting"));
    state.mark_servers_initializing(vec![server.to_string()]);
}

fn announce_server(actor: &SessionActor, server: &str) {
    use distill_tools::implementations::search_tool::fingerprint_servers;
    use distill_tools::types::tool_index::ServerSummary;
    let summary = ServerSummary {
        name: server.to_string(),
        description: None,
        tool_count: 1,
        tool_names: vec!["echo".to_string()],
    };
    actor.mcp_announcements.lock().fingerprints = fingerprint_servers(&[summary]);
    actor
        .mcp_reminder_dirty
        .store(true, std::sync::atomic::Ordering::Relaxed);
}

fn conversation_text(conv: &[ConversationItem]) -> String {
    conv.iter()
        .map(|item| item.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

fn conversation_mentions_connecting(conv: &[ConversationItem]) -> bool {
    conv.iter().any(|item| {
        item.text_content()
            .contains("MCP servers currently connecting")
    })
}

#[tokio::test(flavor = "current_thread")]
async fn unresolved_reconnect_defers_disconnected_reminder() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            mark_mid_handshake(&actor, "slow").await;
            announce_server(&actor, "slow");

            actor.maybe_inject_mcp_reminder().await;

            let conv = actor.chat_state_handle.get_conversation().await;
            let text = conversation_text(&conv);
            assert!(
                !text.contains("disconnected"),
                "reconnecting servers must not be reported as disconnected: {text}"
            );
            assert!(
                actor
                    .mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed),
                "the deferred reminder must stay armed for the post-resolve re-check"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn pre_init_reconnect_defers_disconnected_reminder() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs = vec![stdio_server("slow")];
            }
            announce_server(&actor, "slow");

            actor.maybe_inject_mcp_reminder().await;

            let text = conversation_text(&actor.chat_state_handle.get_conversation().await);
            assert!(
                !text.contains("disconnected"),
                "a restored server must not be announced disconnected before init starts: {text}"
            );
            assert!(
                actor
                    .mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed),
                "the deferred reminder must stay armed until init resolves"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn abandoned_init_defers_disconnected_reminder() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs = vec![stdio_server("slow")];
                let owner = state.try_start_init().expect("fixture must enter Starting");
                state.mark_servers_initializing(vec!["slow".to_string()]);
                drop(owner);
                state.record_init_failure("slow", false, Some("timed out".to_string()));
                assert!(
                    state.is_init_abandoned() && !state.is_initializing(),
                    "fixture must be abandoned, not live-owned"
                );
            }
            announce_server(&actor, "slow");

            actor.maybe_inject_mcp_reminder().await;

            let text = conversation_text(&actor.chat_state_handle.get_conversation().await);
            assert!(
                !text.contains("disconnected"),
                "abandoned init must not announce restored servers as disconnected: {text}"
            );
            assert!(
                actor
                    .mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed),
                "the deferred reminder must stay armed for the post-resolve re-check"
            );

            {
                let mut state = actor.mcp_state.lock().await;
                state.mark_server_ready("slow");
                state.finish_init();
                state.complete_init();
            }
            actor.maybe_inject_mcp_reminder().await;
            let text = conversation_text(&actor.chat_state_handle.get_conversation().await);
            assert!(
                text.contains("disconnected") || text.contains("failed to connect"),
                "post-resolve re-check must announce the settled set: {text}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_set_change_injects_once_init_resolves() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            mark_mid_handshake(&actor, "slow").await;
            announce_server(&actor, "slow");
            actor.maybe_inject_mcp_reminder().await;
            assert!(
                actor
                    .mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed)
            );

            {
                let mut state = actor.mcp_state.lock().await;
                state.record_init_failure("slow", false, Some("timed out".to_string()));
                state.mark_server_ready("slow");
                state.finish_init();
                state.complete_init();
            }
            actor.maybe_inject_mcp_reminder().await;

            let text = conversation_text(&actor.chat_state_handle.get_conversation().await);
            assert!(
                text.contains("disconnected") || text.contains("failed to connect"),
                "post-resolve re-check must announce the settled set: {text}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resolved_disconnect_is_still_reported() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            {
                let mut state = actor.mcp_state.lock().await;
                state.configs = vec![stdio_server("slow")];
                std::mem::forget(state.try_start_init().expect("fixture must enter Starting"));
                state.finish_init();
                state.complete_init();
            }
            announce_server(&actor, "slow");

            actor.maybe_inject_mcp_reminder().await;

            let conv = actor.chat_state_handle.get_conversation().await;
            let text = conversation_text(&conv);
            assert!(
                text.contains("disconnected") || text.contains("failed to connect"),
                "a real post-resolve disconnect must reach the model: {text}"
            );
        })
        .await;
}

#[test]
fn blocking_prompt_mid_handshake_gets_no_stale_connecting_reminder() {
    block_on_session_turn(|| async {
        let actor = actor_with_persistence_drain().await;
        mark_mid_handshake(&actor, "slow").await;
        actor.mcp_strategy.set(McpInitStrategy::Blocking);

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let actor_for_prompt = actor.clone();
        let prompt_task = tokio::task::spawn_local(async move {
            actor_for_prompt
                .handle_prompt(
                    "gb5503-blocking",
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "hi".to_string(),
                    ))],
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    /* verbatim */ false,
                    /* send_now */ false,
                    None,
                    Some(ack_tx),
                    None,
                )
                .await
        });
        // Persist-ack is after turn-start reminders and the user item, before the model call.
        assert!(ack_rx.await.is_ok());

        let conv = actor.chat_state_handle.get_conversation().await;
        assert!(
            !conversation_mentions_connecting(&conv),
            "Blocking strategy: the connecting reminder must be deferred \
                 past the handshake wait, not injected at turn start"
        );
        prompt_task.abort();
    });
}

#[test]
fn progressive_prompt_mid_handshake_still_announces_connecting() {
    block_on_session_turn(|| async {
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let actor = actor_with_persistence_drain_and_sampler(
            distill_sampler::SamplerHandle::notify_on_submit(std::sync::Arc::clone(&notify)),
        )
        .await;
        mark_mid_handshake(&actor, "slow").await;
        actor.mcp_strategy.set(McpInitStrategy::Progressive);

        let submitted = notify.notified();
        tokio::pin!(submitted);
        submitted.as_mut().enable();

        let (ack_tx, _ack_rx) = tokio::sync::oneshot::channel();
        let actor_for_prompt = actor.clone();
        let prompt_task = tokio::task::spawn_local(async move {
            actor_for_prompt
                .handle_prompt(
                    "gb5503-progressive",
                    vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "hi".to_string(),
                    ))],
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    /* verbatim */ false,
                    /* send_now */ false,
                    None,
                    Some(ack_tx),
                    None,
                )
                .await
        });

        // Reminder is injected after tool prep, before the first sampling submit.
        tokio::time::timeout(std::time::Duration::from_secs(5), submitted)
            .await
            .expect("first sampling submit");
        let conv = actor.chat_state_handle.get_conversation().await;
        assert!(
            conversation_mentions_connecting(&conv),
            "Progressive strategy must still announce mid-handshake servers"
        );
        prompt_task.abort();
    });
}
