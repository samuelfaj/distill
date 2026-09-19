// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;

fn install_client_hook(
    actor: &SessionActor,
    event: distill_hooks::event::HookEventName,
    callback_ids: &[&str],
) {
    let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
    client_hooks.insert(
        event,
        vec![crate::extensions::hooks::ClientHookGroup {
            matcher: None,
            callback_ids: callback_ids.iter().map(|s| s.to_string()).collect(),
            timeout: None,
        }],
    );
    *actor.client_hooks.borrow_mut() = client_hooks;
}

async fn test_actor() -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
    tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    (actor, gateway_rx, persistence_rx)
}

fn post_tool_use_envelope(actor: &SessionActor) -> distill_hooks::event::HookEventEnvelope {
    actor.make_hook_envelope(
        distill_hooks::event::HookEventName::PostToolUse,
        None,
        distill_hooks::event::HookPayload::PostToolUse {
            tool_name: "run_terminal_command".to_string(),
            tool_use_id: "call_1".to_string(),
            tool_input: serde_json::json!({}),
            tool_result: serde_json::json!({}),
            tool_input_truncated: false,
            tool_result_truncated: false,
            duration_ms: None,
            is_backgrounded: false,
            subagent_type: None,
        },
    )
}

fn spawn_run_responder(
    mut gateway_rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
    reply: impl Fn(&serde_json::Value) -> serde_json::Value + 'static,
) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            match msg {
                distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                    let params: serde_json::Value =
                        serde_json::from_str(args.request.params.get()).unwrap();
                    let body: Arc<serde_json::value::RawValue> =
                        serde_json::value::to_raw_value(&reply(&params))
                            .unwrap()
                            .into();
                    let _ = args.response_tx.send(Ok(acp::ExtResponse::new(body)));
                }
                distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                _ => {}
            }
        }
    });
}

fn spawn_deny_responder(
    gateway_rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
    reason: &'static str,
) {
    spawn_run_responder(
        gateway_rx,
        move |_| serde_json::json!({ "decision": "deny", "systemMessage": reason }),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn client_hooks_fire_without_file_registry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx, _persistence_rx) = test_actor().await;

            assert!(
                actor.hook_registry.borrow().is_none(),
                "fixture must have no file registry for this invariant"
            );
            install_client_hook(&actor, distill_hooks::event::HookEventName::Stop, &["cb_0"]);

            actor.fire_hook(
                distill_hooks::event::HookEventName::Stop,
                None,
                distill_hooks::event::HookPayload::Stop {
                    reason: "end_turn".to_string(),
                    stop_hook_active: false,
                    last_assistant_message: None,
                    background_tasks: None,
                    session_crons: None,
                },
            );

            let msg = gateway_rx
                .try_recv()
                .expect("client hook must fire with no file registry");
            let distill_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                panic!("expected an x.ai/hooks/event ext notification");
            };
            assert_eq!(args.request.method.as_ref(), "x.ai/hooks/event");
            let params: serde_json::Value =
                serde_json::from_str(args.request.params.get()).unwrap();
            assert_eq!(
                params
                    .pointer("/hookCallbackId")
                    .unwrap_or(&serde_json::Value::Null),
                "cb_0"
            );
            assert_eq!(
                params
                    .pointer("/hookEventName")
                    .unwrap_or(&serde_json::Value::Null),
                "stop"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn pre_tool_use_resolves_meta_dispatch_tool_name_end_to_end() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;
            *actor.agent.borrow_mut() = test_agent_with_tools(vec![
                distill_tools::registry::types::ToolConfig::for_tool::<
                    distill_tools::implementations::use_tool::UseTool,
                >(),
            ])
            .await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            client_hooks.insert(
                distill_hooks::event::HookEventName::PreToolUse,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: Some(
                        distill_hooks::matcher::HookMatcher::new("linear__save_issue").unwrap(),
                    ),
                    callback_ids: vec!["cb_0".to_string()],
                    timeout: None,
                }],
            );
            *actor.client_hooks.borrow_mut() = client_hooks;
            spawn_deny_responder(gateway_rx, "nope");

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "use_tool",
                    r#"{"tool_name":"linear__save_issue","tool_input":{}}"#,
                ),
            };

            let mut deferred = Vec::new();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.prepare_tool_call(call, &mut deferred),
            )
            .await
            .expect("prepare_tool_call must not hang")
            .expect("prepare_tool_call must not error");
            assert!(
                matches!(result, Err(ToolLoop::HookDenied { .. })),
                "a hook matched on the resolved tool must gate the use_tool dispatch; \
                 got {result:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn subagent_inherits_parent_pre_tool_use_client_hook() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (parent, _parent_gateway_rx, _parent_persistence_rx) = test_actor().await;

            install_client_hook(
                &parent,
                distill_hooks::event::HookEventName::PreToolUse,
                &["cb_0"],
            );

            let (subagent, mut child_gateway_rx, _child_persistence_rx) = test_actor().await;

            assert!(
                subagent.client_hooks.borrow().is_empty(),
                "the subagent starts with no hooks of its own"
            );
            *subagent.client_hooks.borrow_mut() = parent.client_hooks.borrow().clone();

            let seen_subagent_type = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
            let seen = seen_subagent_type.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = child_gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let params: serde_json::Value =
                                serde_json::from_str(args.request.params.get()).unwrap();
                            *seen.lock().unwrap() = params
                                .pointer("/subagentType")
                                .unwrap_or(&serde_json::Value::Null)
                                .as_str()
                                .map(str::to_string);
                            let deny: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({
                                    "decision": "deny",
                                }))
                                .unwrap()
                                .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "run_terminal_command",
                    "{}",
                ),
            };
            let tool_call_id = acp::ToolCallId::new("call_1");
            let envelope = subagent.make_hook_envelope(
                distill_hooks::event::HookEventName::PreToolUse,
                None,
                distill_hooks::event::HookPayload::PreToolUse {
                    tool_name: call.function.name.clone(),
                    tool_use_id: call.id.clone(),
                    tool_input: serde_json::json!({}),
                    tool_input_truncated: false,
                    subagent_type: Some("code-reviewer".to_string()),
                },
            );

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                subagent.run_pre_tool_use_client_hook(&call, &tool_call_id, &envelope),
            )
            .await
            .expect("the gate must not hang")
            .expect("the gate must not error");

            assert!(
                matches!(result, Some(ToolLoop::HookDenied { .. })),
                "a subagent tool call must be blocked by the parent's inherited PreToolUse hook"
            );
            assert_eq!(
                seen_subagent_type.lock().unwrap().as_deref(),
                Some("code-reviewer"),
                "the parent's hook must observe the subagent's type on the dispatch"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn pre_tool_use_slow_callback_does_not_starve_a_deny() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx, _persistence_rx) = test_actor().await;

            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::PreToolUse,
                &["slow_cb", "deny_cb"],
            );

            tokio::task::spawn_local(async move {
                let mut held = Vec::new();
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let params: serde_json::Value =
                                serde_json::from_str(args.request.params.get()).unwrap();
                            if params
                                .pointer("/hookCallbackId")
                                .unwrap_or(&serde_json::Value::Null)
                                == "deny_cb"
                            {
                                let deny: Arc<serde_json::value::RawValue> =
                                    serde_json::value::to_raw_value(&serde_json::json!({
                                        "decision": "deny",
                                    }))
                                    .unwrap()
                                    .into();
                                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                            } else {
                                held.push(args.response_tx);
                            }
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "run_terminal_command",
                    "{}",
                ),
            };
            let tool_call_id = acp::ToolCallId::new("call_1");
            let envelope = actor.make_hook_envelope(
                distill_hooks::event::HookEventName::PreToolUse,
                None,
                distill_hooks::event::HookPayload::PreToolUse {
                    tool_name: call.function.name.clone(),
                    tool_use_id: call.id.clone(),
                    tool_input: serde_json::json!({}),
                    tool_input_truncated: false,
                    subagent_type: None,
                },
            );

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_pre_tool_use_client_hook(&call, &tool_call_id, &envelope),
            )
            .await
            .expect("a deny must resolve without waiting on the hung callback")
            .expect("the gate must not error");
            assert!(matches!(result, Some(ToolLoop::HookDenied { .. })));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_and_failure_never_double_fire() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx, _persistence_rx) = test_actor().await;
            *actor.agent.borrow_mut() = test_distill_agent_with_todo().await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            for event in [
                distill_hooks::event::HookEventName::PostToolUse,
                distill_hooks::event::HookEventName::PostToolUseFailure,
            ] {
                client_hooks.insert(
                    event,
                    vec![crate::extensions::hooks::ClientHookGroup {
                        matcher: None,
                        callback_ids: vec!["cb".to_string()],
                        timeout: None,
                    }],
                );
            }
            *actor.client_hooks.borrow_mut() = client_hooks;

            let todo_call = |id: &str| crate::sampling::types::ToolCallResponse {
                id: id.to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "todo_write",
                    r#"{"todos":[{"id":"t1","content":"do","status":"completed"}]}"#,
                ),
            };

            actor
                .execute_tool_calls(vec![todo_call("call_err")])
                .await
                .expect("execute_tool_calls must not error");
            let mut failure_events = Vec::new();
            while let Ok(msg) = gateway_rx.try_recv() {
                if let distill_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                    && args.request.method.as_ref() == "x.ai/hooks/event"
                {
                    let params: serde_json::Value =
                        serde_json::from_str(args.request.params.get()).unwrap();
                    if let Some(name) = params
                        .pointer("/hookEventName")
                        .unwrap_or(&serde_json::Value::Null)
                        .as_str()
                    {
                        failure_events.push(name.to_string());
                    }
                }
            }
            assert_eq!(
                failure_events,
                ["post_tool_use_failure"],
                "an errored tool must fire only PostToolUseFailure, never PostToolUse"
            );

            let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let seen_task = seen.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            if args.request.method.as_ref() == "x.ai/hooks/run" {
                                let params: serde_json::Value =
                                    serde_json::from_str(args.request.params.get()).unwrap();
                                if let Some(name) = params
                                    .pointer("/hookEventName")
                                    .unwrap_or(&serde_json::Value::Null)
                                    .as_str()
                                {
                                    seen_task.lock().unwrap().push(name.to_string());
                                }
                            }
                            let empty: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({}))
                                    .unwrap()
                                    .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(empty)));
                        }
                        distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
                            if args.request.method.as_ref() == "x.ai/hooks/event" {
                                let params: serde_json::Value =
                                    serde_json::from_str(args.request.params.get()).unwrap();
                                if let Some(name) = params
                                    .pointer("/hookEventName")
                                    .unwrap_or(&serde_json::Value::Null)
                                    .as_str()
                                {
                                    seen_task.lock().unwrap().push(name.to_string());
                                }
                            }
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            actor
                .workspace_ops
                .bind_local_session(
                    &actor.session_id_string(),
                    actor.tool_context.cwd.as_path().to_path_buf(),
                    actor.tool_context.hunk_tracker_handle.clone(),
                    actor.agent.borrow().tool_bridge().toolset(),
                    None,
                )
                .expect("bind_local_session must succeed");
            actor
                .execute_tool_calls(vec![todo_call("call_ok")])
                .await
                .expect("execute_tool_calls must not error");
            assert_eq!(
                *seen.lock().unwrap(),
                ["post_tool_use"],
                "a successful tool must fire PostToolUse exactly once, never PostToolUseFailure"
            );
        })
        .await;
}

/// Stub tool whose result is an MCP error output (`Ok`, but `is_error`): the
/// shape that routes to `PostToolUseFailure` instead of `PostToolUse`.
#[derive(Debug)]
struct McpErrorResultTool;

impl distill_tools::types::tool_metadata::ToolMetadata for McpErrorResultTool {
    fn kind(&self) -> distill_tools::types::tool::ToolKind {
        distill_tools::types::tool::ToolKind::Other
    }
    fn tool_namespace(&self) -> distill_tools::types::tool::ToolNamespace {
        distill_tools::types::tool::ToolNamespace::MCP
    }
    fn description_template(&self) -> &str {
        "stub MCP tool that returns an error result"
    }
}

impl distill_tool_runtime::Tool for McpErrorResultTool {
    type Args = serde_json::Value;
    type Output = distill_tools::types::output::ToolOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new("mock_error_tool").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new("mock_error_tool", "stub MCP error tool")
    }

    async fn run(
        &self,
        _ctx: distill_tool_runtime::ToolCallContext,
        _args: serde_json::Value,
    ) -> Result<Self::Output, distill_tool_runtime::ToolError> {
        Ok(distill_tools::types::output::ToolOutput::MCP(
            distill_tools::types::output::MCPOutput::errored(
                "mock_error_tool".into(),
                "mock".into(),
                "upstream exploded".into(),
            ),
        ))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_error_result_fires_only_failure_and_delivers_original_output() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx, _persistence_rx) = test_actor().await;
            *actor.agent.borrow_mut() = test_distill_agent_with_todo().await;
            let bridge = actor.agent.borrow().tool_bridge().clone();
            bridge
                .register_mcp_tools(
                    "mock_error_tool".to_string(),
                    McpErrorResultTool,
                    Some(serde_json::json!({ "type": "object" })),
                )
                .expect("stub tool registration must succeed");

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            for event in [
                distill_hooks::event::HookEventName::PostToolUse,
                distill_hooks::event::HookEventName::PostToolUseFailure,
            ] {
                client_hooks.insert(
                    event,
                    vec![crate::extensions::hooks::ClientHookGroup {
                        matcher: None,
                        callback_ids: vec!["cb".to_string()],
                        timeout: None,
                    }],
                );
            }
            *actor.client_hooks.borrow_mut() = client_hooks;

            let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let seen_task = seen.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            if args.request.method.as_ref() == "x.ai/hooks/run" {
                                let params: serde_json::Value =
                                    serde_json::from_str(args.request.params.get()).unwrap();
                                if let Some(name) = params.pointer("/hookEventName").unwrap_or(&serde_json::Value::Null).as_str() {
                                    seen_task.lock().unwrap().push(name.to_string());
                                }
                            }
                            let empty: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({}))
                                    .unwrap()
                                    .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(empty)));
                        }
                        distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
                            if args.request.method.as_ref() == "x.ai/hooks/event" {
                                let params: serde_json::Value =
                                    serde_json::from_str(args.request.params.get()).unwrap();
                                if let Some(name) = params.pointer("/hookEventName").unwrap_or(&serde_json::Value::Null).as_str() {
                                    seen_task.lock().unwrap().push(name.to_string());
                                }
                            }
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            actor
                .workspace_ops
                .bind_local_session(
                    &actor.session_id_string(),
                    actor.tool_context.cwd.as_path().to_path_buf(),
                    actor.tool_context.hunk_tracker_handle.clone(),
                    actor.agent.borrow().tool_bridge().toolset(),
                    None,
                )
                .expect("bind_local_session must succeed");

            let call = crate::sampling::types::ToolCallResponse {
                id: "call_mcp_err".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new("mock_error_tool", "{}"),
            };
            actor
                .execute_tool_calls(vec![call])
                .await
                .expect("execute_tool_calls must not error");
            // Let the observe-only failure notification drain to the recorder.
            for _ in 0..10 {
                if !seen.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }

            assert_eq!(
                *seen.lock().unwrap(),
                ["post_tool_use_failure"],
                "an MCP error result must fire only PostToolUseFailure, never PostToolUse"
            );

            let conversation = actor.chat_state_handle.get_conversation().await;
            assert!(
                conversation
                    .iter()
                    .any(|item| format!("{item:?}").contains("upstream exploded")),
                "the original MCP error output must reach the model unchanged, got: {conversation:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_failure_additional_context_reaches_model() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut actor, _gateway_rx, _persistence_rx) = test_actor().await;
            *actor.agent.borrow_mut() = test_distill_agent_with_todo().await;

            // A file PostToolUseFailure hook feeds additionalContext (context-only).
            install_pre_tool_use_hooks(
                &mut actor,
                vec![post_tool_use_failure_spec(
                    "recover_hook",
                    None,
                    r#"echo '{"hookSpecificOutput":{"additionalContext":"retry after binding"}}'"#,
                )],
            );

            // todo_write before a bound workspace fails to dispatch (Err arm).
            let failing_call = crate::sampling::types::ToolCallResponse {
                id: "call_err".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "todo_write",
                    r#"{"todos":[{"id":"t1","content":"do","status":"completed"}]}"#,
                ),
            };

            actor
                .execute_tool_calls(vec![failing_call])
                .await
                .expect("execute_tool_calls must not error");

            let conversation = actor.chat_state_handle.get_conversation().await;
            let failed_result_pos = conversation
                .iter()
                .position(|item| {
                    matches!(item, ConversationItem::ToolResult(tr) if tr.tool_call_id.as_str() == "call_err")
                })
                .expect("the failed tool result must be in the conversation");
            let note_pos = conversation
                .iter()
                .position(|item| format!("{item:?}").contains("retry after binding"))
                .expect("the failure additionalContext note must be in the conversation");
            assert!(
                note_pos > failed_result_pos,
                "the PostToolUseFailure additionalContext note must appear AFTER the failed tool result \
                 (failed_result_pos={failed_result_pos}, note_pos={note_pos}), conversation: {conversation:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn pre_tool_use_deny_feeds_reason_back_and_continues_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;
            *actor.agent.borrow_mut() = test_distill_agent_with_todo().await;

            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::PreToolUse,
                &["cb_0"],
            );
            spawn_deny_responder(gateway_rx, "use read_file instead");

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "todo_write",
                    r#"{"todos":[{"id":"t1","content":"do","status":"completed"}]}"#,
                ),
            };

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.execute_tool_calls(vec![call]),
            )
            .await
            .expect("execute_tool_calls must not hang")
            .expect("execute_tool_calls must not error");

            assert!(
                matches!(result, ToolLoop::Continue),
                "a pre_tool_use deny must continue the turn, got {result:?}"
            );

            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                conv.iter()
                    .any(|c| c.text_content().contains("use read_file instead")),
                "the deny reason must be fed back as the tool_result"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn stop_client_gate_maps_deny_continue_false_and_context() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;

            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::Stop,
                &["cb_block", "cb_stop", "cb_ctx"],
            );

            spawn_run_responder(gateway_rx, |params| {
                match params
                    .pointer("/hookCallbackId")
                    .unwrap_or(&serde_json::Value::Null)
                    .as_str()
                {
                    Some("cb_block") => serde_json::json!({
                        "decision": "deny",
                        "systemMessage": "finish the tests first",
                    }),
                    Some("cb_stop") => {
                        serde_json::json!({ "continue": false, "stopReason": "budget" })
                    }
                    _ => serde_json::json!({ "additionalContext": "run the linter" }),
                }
            });

            let envelope = actor.make_hook_envelope(
                distill_hooks::event::HookEventName::Stop,
                Some("prompt-1".to_string()),
                distill_hooks::event::HookPayload::Stop {
                    reason: "end_turn".to_string(),
                    stop_hook_active: false,
                    last_assistant_message: None,
                    background_tasks: None,
                    session_crons: None,
                },
            );
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_client_hooks(&envelope),
            )
            .await
            .expect("the stop gate must not hang");

            let [block] = result.blocks.as_slice() else {
                panic!("only the deny becomes a block: {:?}", result.blocks);
            };
            assert_eq!(block.hook_name, "client:cb_block");
            assert_eq!(block.reason, "finish the tests first");
            let prevent = result
                .prevent_continuation
                .expect("continue:false becomes prevent_continuation");
            assert_eq!(prevent.hook_name, "client:cb_stop");
            assert_eq!(prevent.reason, "budget");
            assert_eq!(result.additional_context, ["run the linter"]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_client_gate_contributes_block_and_context() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;

            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::PostToolUse,
                &["cb_block", "cb_ctx"],
            );

            spawn_run_responder(gateway_rx, |params| {
                if params
                    .pointer("/hookCallbackId")
                    .unwrap_or(&serde_json::Value::Null)
                    == "cb_block"
                {
                    serde_json::json!({ "decision": "block", "reason": "revert that edit" })
                } else {
                    serde_json::json!({ "additionalContext": "run the linter" })
                }
            });

            let envelope = post_tool_use_envelope(&actor);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_post_tool_use_client_hooks(&envelope),
            )
            .await
            .expect("the post_tool_use gate must not hang");

            assert_eq!(result.blocks.len(), 1, "only the denying callback blocks");
            let [block] = result.blocks.as_slice() else {
                panic!("expected one block: {:?}", result.blocks);
            };
            assert_eq!(block.hook_name, "client:cb_block");
            assert_eq!(block.reason, "revert that edit");
            assert_eq!(result.additional_context.len(), 1);
            let [ctx] = result.additional_context.as_slice() else {
                panic!(
                    "expected one additional context: {:?}",
                    result.additional_context
                );
            };
            assert_eq!(ctx.hook_name, "client:cb_ctx");
            assert_eq!(ctx.text, "run the linter");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_client_gate_records_failure_and_orders_contributions() {
    use distill_hooks::result::HookRunResult;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx, _persistence_rx) = test_actor().await;

            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::PostToolUse,
                &["cb_ctx_a", "cb_fail", "cb_ctx_b"],
            );

            tokio::task::spawn_local(async move {
                let mut buffered = Vec::new();
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            buffered.push(args);
                            if buffered.len() == 3 {
                                while let Some(args) = buffered.pop() {
                                    let params: serde_json::Value =
                                        serde_json::from_str(args.request.params.get()).unwrap();
                                    let reply: Result<acp::ExtResponse, acp::Error> = if params
                                        .pointer("/hookCallbackId")
                                        .unwrap_or(&serde_json::Value::Null)
                                        == "cb_fail"
                                    {
                                        Err(acp::Error::internal_error())
                                    } else {
                                        let value = if params
                                            .pointer("/hookCallbackId")
                                            .unwrap_or(&serde_json::Value::Null)
                                            == "cb_ctx_a"
                                        {
                                            serde_json::json!({ "additionalContext": "first" })
                                        } else {
                                            serde_json::json!({ "additionalContext": "second" })
                                        };
                                        let response_params: Arc<serde_json::value::RawValue> =
                                            serde_json::value::to_raw_value(&value).unwrap().into();
                                        Ok(acp::ExtResponse::new(response_params))
                                    };
                                    let _ = args.response_tx.send(reply);
                                }
                            }
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let envelope = post_tool_use_envelope(&actor);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_post_tool_use_client_hooks(&envelope),
            )
            .await
            .expect("the post_tool_use gate must not hang");

            let context: Vec<(&str, &str)> = result
                .additional_context
                .iter()
                .map(|c| (c.hook_name.as_str(), c.text.as_str()))
                .collect();
            assert_eq!(
                context,
                [("client:cb_ctx_a", "first"), ("client:cb_ctx_b", "second")]
            );

            let results: Vec<(&str, bool)> = result
                .results
                .iter()
                .map(|r| match r {
                    HookRunResult::Failed { hook_name, .. } => (hook_name.as_str(), true),
                    HookRunResult::Success { hook_name, .. } => (hook_name.as_str(), false),
                    _ => panic!("post_tool_use client results are only Success or Failed"),
                })
                .collect();
            assert_eq!(
                results,
                [
                    ("client:cb_ctx_a", false),
                    ("client:cb_fail", true),
                    ("client:cb_ctx_b", false),
                ]
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_dispatch_merges_file_then_client_contributions() {
    use distill_tools::types::output::{MCPOutput, ToolOutput, ToolRunResult};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut actor, gateway_rx, _persistence_rx) = test_actor().await;

            install_pre_tool_use_hooks(
                &mut actor,
                vec![post_tool_use_spec(
                    "file_hook",
                    None,
                    r#"echo '{"decision":"block","reason":"file block","hookSpecificOutput":{"additionalContext":"file context"}}'"#,
                )],
            );
            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::PostToolUse,
                &["cb_client"],
            );

            spawn_run_responder(gateway_rx, |_| {
                serde_json::json!({
                    "decision": "block",
                    "reason": "client block",
                    "additionalContext": "client context",
                })
            });

            let run = ToolRunResult {
                output: ToolOutput::MCP(MCPOutput::okay_output(
                    "search".into(),
                    "memory".into(),
                    "original".into(),
                )),
                prompt_text: "original".into(),
                effective_tool_name: None,
            };
            let drained = DrainedToolSuccess::new(run);
            let prepared = PreparedToolCall {
                call_id: "call_1".to_string(),
                tool_call_id: acp::ToolCallId::new("call_1"),
                tool_name: "search__memory".to_string(),
                raw_arguments: "{}".to_string(),
                parsed_args: serde_json::json!({}),
                model_id: "test-model".to_string(),
                concatenated_json_count: 0,
                dispatch_target_name: None,
                is_read_only: false,
                rewriting_hook: None,
                additional_context: Vec::new(),
            };

            let (delivery, _scrollback) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.dispatch_post_tool_use_hook(&prepared, drained.output(), None),
            )
            .await
            .expect("the post_tool_use dispatch must not hang");

            let blocks: Vec<(&str, &str)> = delivery
                .blocks
                .iter()
                .map(|b| (b.hook_name.as_str(), b.reason.as_str()))
                .collect();
            assert_eq!(
                blocks,
                [
                    ("file_hook", "file block"),
                    ("client:cb_client", "client block"),
                ],
                "file block precedes the client block in the merged result"
            );

            let context: Vec<(&str, &str)> = delivery
                .additional_context
                .iter()
                .map(|c| (c.hook_name.as_str(), c.text.as_str()))
                .collect();
            assert_eq!(
                context,
                [
                    ("file_hook", "file context"),
                    ("client:cb_client", "client context"),
                ],
                "file context precedes the client context in the merged result"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn run_stop_gate_keep_working_and_cap() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;

            let decision = actor.run_stop_gate("prompt-1", 0).await;
            assert!(matches!(decision, StopGateDecision::AllowStop));

            install_client_hook(&actor, distill_hooks::event::HookEventName::Stop, &["cb_0"]);

            spawn_deny_responder(gateway_rx, "keep working");

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-1", 0),
            )
            .await
            .expect("the stop gate must not hang");
            match decision {
                StopGateDecision::KeepWorking { feedback } => {
                    assert!(
                        feedback.contains("Stop hook feedback:")
                            && feedback.contains("keep working"),
                        "feedback must carry the deny message, got: {feedback}"
                    );
                }
                _ => panic!("a client deny must keep the agent working"),
            }

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-1", MAX_STOP_HOOK_CONTINUATIONS_PER_TURN),
            )
            .await
            .expect("the capped gate must not hang");
            assert!(matches!(decision, StopGateDecision::AllowStop));
        })
        .await;
}

pub(super) fn file_registry_with_spec(
    event: distill_hooks::event::HookEventName,
    script: &str,
) -> distill_hooks::discovery::HookRegistry {
    file_registry(event, script, true)
}

fn file_registry(
    event: distill_hooks::event::HookEventName,
    script: &str,
    enabled: bool,
) -> distill_hooks::discovery::HookRegistry {
    let (mut registry, _) = distill_hooks::discovery::load_hooks(None, None);
    registry.append_specs(vec![distill_hooks::config::HookSpec {
        name: "test/stop-hook".into(),
        event,
        handler_type: distill_hooks::config::HandlerType::Command,
        configured_matcher: None,
        matcher: None,
        enabled,
        command: Some(std::path::PathBuf::from(script)),
        command_raw: Some(script.to_string()),
        url: None,
        url_raw: None,
        timeout_ms: 5000,
        source_dir: std::path::PathBuf::from("/tmp"),
        extra_env: std::collections::HashMap::new(),
        layer: distill_hooks::config::HookProvenance::File,
    }]);
    registry
}

#[tokio::test(flavor = "current_thread")]
async fn file_force_stop_skips_client_gate_but_notifies() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut actor, mut gateway_rx, _persistence_rx) = test_actor().await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();

            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(file_registry_with_spec(
                distill_hooks::event::HookEventName::Stop,
                r#"echo '{"continue":false,"stopReason":"budget exhausted"}'"#,
            )));
            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::Stop,
                &["cb_observer"],
            );

            let run_requests = std::rc::Rc::new(std::cell::Cell::new(0u32));
            let observe_events = std::rc::Rc::new(std::cell::Cell::new(0u32));
            let (runs, observes) = (run_requests.clone(), observe_events.clone());
            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            if args.request.method.as_ref() == "x.ai/hooks/run" {
                                runs.set(runs.get() + 1);
                            }
                            let empty: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({}))
                                    .unwrap()
                                    .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(empty)));
                        }
                        distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
                            if args.request.method.as_ref() == "x.ai/hooks/event" {
                                observes.set(observes.get() + 1);
                            }
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-1", 0),
            )
            .await
            .expect("the stop gate must not hang");
            assert!(
                matches!(decision, StopGateDecision::AllowStop),
                "a file force-stop must end the turn"
            );
            tokio::task::yield_now().await;
            assert_eq!(run_requests.get(), 0, "the client run gate must be skipped");
            assert_eq!(
                observe_events.get(),
                1,
                "client callbacks must still see the turn end as an observe event"
            );
            assert!(actor.turn_report.claim_for_gate().is_none());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn client_force_stop_attribution_is_registration_ordered() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx, _persistence_rx) = test_actor().await;

            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::Stop,
                &["cb_first", "cb_second"],
            );

            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        distill_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let params: serde_json::Value =
                                serde_json::from_str(args.request.params.get()).unwrap();
                            let is_first = params
                                .pointer("/hookCallbackId")
                                .unwrap_or(&serde_json::Value::Null)
                                == "cb_first";
                            tokio::task::spawn_local(async move {
                                if is_first {
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                }
                                let reason = if is_first {
                                    "from-first"
                                } else {
                                    "from-second"
                                };
                                let body: Arc<serde_json::value::RawValue> =
                                    serde_json::value::to_raw_value(&serde_json::json!({
                                        "continue": false,
                                        "stopReason": reason,
                                    }))
                                    .unwrap()
                                    .into();
                                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(body)));
                            });
                        }
                        distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let envelope = actor.make_hook_envelope(
                distill_hooks::event::HookEventName::Stop,
                Some("prompt-1".to_string()),
                distill_hooks::event::HookPayload::Stop {
                    reason: "end_turn".to_string(),
                    stop_hook_active: false,
                    last_assistant_message: None,
                    background_tasks: None,
                    session_crons: None,
                },
            );
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_client_hooks(&envelope),
            )
            .await
            .expect("the stop gate must not hang");

            let prevent = result.prevent_continuation.expect("force-stop captured");
            assert_eq!(
                prevent.hook_name, "client:cb_first",
                "attribution must follow registration order, not completion order"
            );
            assert_eq!(prevent.reason, "from-first");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn subagent_session_gates_on_subagent_stop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut actor, mut gateway_rx, _persistence_rx) = test_actor().await;
            actor.startup_hints.is_subagent = true;
            actor.hook_resolved_workspace_root = "/tmp".to_string();

            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(file_registry_with_spec(
                distill_hooks::event::HookEventName::SubagentStop,
                r#"echo '{"decision":"block","reason":"verify the summary"}'"#,
            )));

            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    if let distill_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                        let _ = args.response_tx.send(Ok(()));
                    }
                }
            });

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-1", 0),
            )
            .await
            .expect("the subagent stop gate must not hang");
            match decision {
                StopGateDecision::KeepWorking { feedback } => {
                    assert!(
                        feedback.contains("verify the summary"),
                        "the SubagentStop block reason must become feedback, got: {feedback}"
                    );
                }
                other => {
                    panic!("a SubagentStop block must keep the subagent working, got {other:?}")
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn alias_envelope_serializes_canonical_event_name() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx, _persistence_rx) = test_actor().await;

            let envelope = actor.make_hook_envelope(
                distill_hooks::event::HookEventName::SubagentEnd,
                None,
                distill_hooks::event::HookPayload::SubagentStop {
                    phase: distill_hooks::event::SubagentStopPhase::Observe,
                    subagent_id: "sub-1".into(),
                    subagent_type: "explore".into(),
                    stop_hook_active: None,
                    last_assistant_message: None,
                },
            );
            let value = serde_json::to_value(&envelope).expect("envelope serializes");
            assert_eq!(
                value
                    .pointer("/hookEventName")
                    .unwrap_or(&serde_json::Value::Null),
                "subagent_stop"
            );
            assert_eq!(
                value
                    .pointer("/permissionMode")
                    .unwrap_or(&serde_json::Value::Null),
                "bypassPermissions"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn client_force_stop_reports_the_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;
            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::Stop,
                &["cb_stop"],
            );

            spawn_run_responder(
                gateway_rx,
                |_| serde_json::json!({ "continue": false, "stopReason": "budget" }),
            );

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-1", 0),
            )
            .await
            .expect("the stop gate must not hang");
            assert!(matches!(decision, StopGateDecision::AllowStop));
            assert!(actor.turn_report.claim_for_gate().is_none());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_unanswered_client_gate_leaves_the_report_unspent() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, mut gateway_rx, _persistence_rx) = test_actor().await;
            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::Stop,
                &["cb_stop"],
            );

            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    if let distill_acp_lib::AcpClientMessage::ExtMethod(args) = msg {
                        drop(args.response_tx);
                    }
                }
            });

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-1", 0),
            )
            .await
            .expect("the stop gate must not hang");

            assert!(matches!(decision, StopGateDecision::AllowStop));
            assert!(
                actor.turn_report.claim_for_gate().is_some(),
                "a gate whose only hook never answered must leave the turn reportable"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_reported_turn_skips_the_gate_entirely() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut actor, _gateway_rx, _persistence_rx) = test_actor().await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(file_registry_with_spec(
                distill_hooks::event::HookEventName::Stop,
                "exit 2",
            )));

            let claim = actor
                .turn_report
                .claim_for_gate()
                .expect("the turn is fresh");
            assert_eq!(
                claim.commit(),
                super::turn_report_slot::CommitOutcome::Reported,
                "an interrupt reported this turn"
            );

            let decision = actor.run_stop_gate("prompt-1", 0).await;
            assert!(
                matches!(decision, StopGateDecision::AllowStop),
                "a hook that ran would have blocked the stop"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn only_a_completed_stop_hook_is_the_turns_report() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut actor, _gateway_rx, _persistence_rx) = test_actor().await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = std::sync::Arc::new(actor);
            let _queue = super::turn_end_hooks::TurnEndQueue::spawn(actor.clone());
            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::StopFailure,
                &["cb_fail"],
            );
            actor.client_hooks.borrow_mut().insert(
                distill_hooks::event::HookEventName::Stop,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: None,
                    callback_ids: vec![],
                    timeout: None,
                }],
            );

            use super::turn_end_hooks::ReportOutcome;
            for (case, script, enabled, reports_later) in [
                (
                    "disabled, so skipped",
                    "exit 0",
                    false,
                    ReportOutcome::Queued,
                ),
                ("crashed", "exit 1", true, ReportOutcome::Queued),
                ("ran", "exit 0", true, ReportOutcome::AlreadyReported),
            ] {
                actor.turn_report.start_next_turn();
                *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(file_registry(
                    distill_hooks::event::HookEventName::Stop,
                    script,
                    enabled,
                )));

                let decision = actor.run_stop_gate(case, 0).await;
                assert!(matches!(decision, StopGateDecision::AllowStop), "{case}");
                let reported = actor.claim_and_queue(
                    case,
                    actor.turn_report.epoch(),
                    super::turn_end_hooks::TurnEnd::Failed {
                        error: distill_hooks::event::StopFailureKind::Unknown,
                        error_details: None,
                        last_assistant_message: None,
                    },
                );
                assert_eq!(reported, reports_later, "{case}");
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_gate_that_keeps_working_releases_the_report() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;
            install_client_hook(
                &actor,
                distill_hooks::event::HookEventName::Stop,
                &["cb_block"],
            );
            spawn_deny_responder(gateway_rx, "keep working");

            let decision = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_stop_gate("prompt-1", 0),
            )
            .await
            .expect("the stop gate must not hang");
            assert!(matches!(decision, StopGateDecision::KeepWorking { .. }));
            assert!(
                actor.turn_report.claim_for_gate().is_some(),
                "a gate that kept the agent working must leave the report slot free"
            );
        })
        .await;
}

/// A pre-tool batch's `HookRunStarted` and `HookExecution` carry the same identity, since both come from one `HookBatch`.
#[tokio::test(flavor = "current_thread")]
async fn pre_tool_batch_start_and_outcome_share_one_identity() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, gateway_rx, _persistence_rx) = test_actor().await;
            let (_acp, xai_updates) = spawn_capturing_gateway_loop(gateway_rx);
            let registry =
                distill_hooks::discovery::registry_from_specs_deduped(vec![pre_tool_use_spec(
                    "test/pre",
                    Some("read_file"),
                    "true",
                )]);
            let envelope = actor.make_pre_tool_use_envelope(
                "read_file",
                "call_1",
                &serde_json::json!({ "target_file": "a.rs" }),
            );
            let ctx = actor.hook_run_ctx();
            let batch = actor.announce_hook_run(&registry, &envelope, &ctx);
            let pre =
                distill_hooks::dispatcher::dispatch_pre_tool_use(&registry, &envelope, &ctx).await;
            actor.send_hook_execution(&batch, &pre.results).await;
            tokio::task::yield_now().await;

            let updates = xai_updates.lock().unwrap().clone();
            let identity = |kind: &str| {
                let u = updates
                    .iter()
                    .find(|u| u["sessionUpdate"] == kind)
                    .unwrap_or_else(|| panic!("no {kind} in {updates:?}"));
                (
                    u["event_name"].clone(),
                    u["tool_name"].clone(),
                    u["prompt_id"].clone(),
                )
            };
            let started = identity("hook_run_started");
            assert_eq!(
                started,
                (
                    serde_json::json!("pre_tool_use"),
                    serde_json::json!("read_file"),
                    serde_json::Value::Null,
                )
            );
            assert_eq!(identity("hook_execution"), started);
        })
        .await;
}
