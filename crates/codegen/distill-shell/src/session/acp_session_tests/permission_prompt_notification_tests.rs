// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use distill_acp_lib::AcpAgentGatewaySender;

use super::support::*;
use super::*;

fn install_notification_client_hook(actor: &SessionActor) {
    let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
    client_hooks.insert(
        distill_hooks::event::HookEventName::Notification,
        vec![crate::extensions::hooks::ClientHookGroup {
            matcher: None,
            callback_ids: vec!["cb_permission".to_string()],
            timeout: None,
        }],
    );
    *actor.client_hooks.borrow_mut() = client_hooks;
}

async fn setup_actor(
    yolo: bool,
    park_until_hook: bool,
) -> (Arc<SessionActor>, Arc<AtomicUsize>, tokio::task::LocalSet) {
    setup_actor_with_pre_tool_use_hook(yolo, park_until_hook, None).await
}

async fn setup_actor_with_pre_tool_use_hook(
    yolo: bool,
    park_until_hook: bool,
    pre_tool_use_script: Option<&str>,
) -> (Arc<SessionActor>, Arc<AtomicUsize>, tokio::task::LocalSet) {
    let local = tokio::task::LocalSet::new();
    let (actor, hooks) = local
        .run_until(async {
            let (gateway_tx, gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let hook_gateway = AcpAgentGatewaySender::new(gateway_tx.clone());
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.agent.borrow_mut() = test_agent_with_tools(read_and_edit_toolset()).await;
            install_permission_manager(&mut actor, yolo, hook_gateway);
            install_notification_client_hook(&actor);
            if let Some(script) = pre_tool_use_script {
                install_pre_tool_use_hooks(
                    &mut actor,
                    vec![pre_tool_use_spec("test/ask", None, script)],
                );
            }
            let actor = Arc::new(actor);
            actor.wire_permission_prompt_notification();
            let hooks = Arc::new(AtomicUsize::new(0));
            spawn_gateway_loop_counting_prompt_hooks(gateway_rx, hooks.clone(), park_until_hook);
            (actor, hooks)
        })
        .await;
    (actor, hooks, local)
}

#[tokio::test(flavor = "current_thread")]
async fn auto_allowed_tool_does_not_fire_permission_prompt_notification() {
    let (actor, hooks, local) = setup_actor(/*yolo=*/ false, /*park_until_hook=*/ false).await;
    local
        .run_until(async {
            let result = prepare_call(&actor, read_file_call("call_safe")).await;
            assert!(
                result.is_ok(),
                "read must auto-allow; got {:?}",
                result.err()
            );
            drain_gateway_turns().await;
            assert_eq!(
                hooks.load(Ordering::SeqCst),
                0,
                "auto-allowed tool permission must not fire permission_prompt"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn real_user_prompt_fires_permission_prompt_notification() {
    let (actor, hooks, local) = setup_actor(/*yolo=*/ false, /*park_until_hook=*/ true).await;
    local
        .run_until(async {
            let result = prepare_call(&actor, search_replace_call("call_unsafe")).await;
            assert!(
                result.is_ok(),
                "prompted allow-once must prepare; got {:?}",
                result.err()
            );
            assert_eq!(
                hooks.load(Ordering::SeqCst),
                1,
                "a real user permission prompt must fire permission_prompt once"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn inherited_handle_second_wire_does_not_steal_parent_hook() {
    let (parent, hooks, local) = setup_actor(/*yolo=*/ false, /*park_until_hook=*/ true).await;
    local
        .run_until(async {
            let (child_gateway_tx, child_gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut child =
                create_test_actor(0, 256_000, 85, child_gateway_tx, persistence_tx).await;
            child.startup_hints.is_subagent = true;
            child.permissions = parent.permissions.clone();
            install_notification_client_hook(&child);
            let child = Arc::new(child);
            child.wire_permission_prompt_notification();
            let (steal_tx, _steal_rx) = tokio::sync::mpsc::unbounded_channel();
            child.permissions.set_user_prompt_notify(steal_tx);
            drop(child_gateway_rx);

            let result = prepare_call(&parent, search_replace_call("call_shared")).await;
            assert!(
                result.is_ok(),
                "prompted allow-once must prepare; got {:?}",
                result.err()
            );
            assert_eq!(
                hooks.load(Ordering::SeqCst),
                1,
                "parent hook must still fire once after a cloned handle tries to re-wire"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn yolo_tool_does_not_fire_permission_prompt_notification() {
    let (actor, hooks, local) = setup_actor(/*yolo=*/ true, /*park_until_hook=*/ false).await;
    local
        .run_until(async {
            let result = prepare_call(&actor, search_replace_call("call_yolo")).await;
            assert!(
                result.is_ok(),
                "yolo must auto-allow; got {:?}",
                result.err()
            );
            drain_gateway_turns().await;
            assert_eq!(
                hooks.load(Ordering::SeqCst),
                0,
                "yolo auto-approve must not fire permission_prompt"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn hook_ask_under_yolo_fires_permission_prompt_notification() {
    let (actor, hooks, local) = setup_actor_with_pre_tool_use_hook(
        /*yolo=*/ true,
        /*park_until_hook=*/ true,
        Some(r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask"}}'"#),
    )
    .await;
    local
        .run_until(async {
            let result = prepare_call(&actor, search_replace_call("call_hook_ask")).await;
            assert!(
                result.is_ok(),
                "the forced prompt was approved, so the call must run; got {:?}",
                result.err()
            );
            assert_eq!(
                hooks.load(Ordering::SeqCst),
                1,
                "a hook ask forces a real prompt, so permission_prompt must fire once"
            );
        })
        .await;
}
