// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;
async fn build_gate_actor() -> SessionActor {
    use distill_tools::implementations::distill::enter_plan_mode::EnterPlanModeTool;
    use distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeTool;
    use distill_tools::registry::types::ToolConfig;
    let (gateway_tx, mut gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    *actor.agent.borrow_mut() = test_agent_with_tools(vec![
        ToolConfig::from_id("Distill:read_file"),
        ToolConfig::from_id("Distill:search_replace"),
        ToolConfig::for_tool::<EnterPlanModeTool>(),
        ToolConfig::for_tool::<ExitPlanModeTool>(),
    ])
    .await;
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let distill_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    actor
}
async fn prepare(
    actor: &SessionActor,
    call: ToolCallResponse,
) -> Result<PreparedToolCall, ToolLoop> {
    let mut deferred = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        actor.prepare_tool_call(call, &mut deferred),
    )
    .await
    .expect("prepare_tool_call must not hang (a hang means a permission prompt was issued)")
    .expect("prepare_tool_call must not error")
}
#[tokio::test(flavor = "current_thread")]
async fn plan_mode_rejects_grok_edit_outside_plan_file_despite_allow_all_permissions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_gate_actor().await;
            activate_plan_mode(&actor);
            let result = prepare(
                &actor,
                search_replace_call_at("call_gate", "/tmp/src/main.rs"),
            )
            .await;
            assert!(
                matches!(result, Err(ToolLoop::Continue)),
                "gate must reject with Continue (tool not executed); got {result:?}"
            );
            let text = tool_result_text(&actor, "call_gate").await;
            assert!(
                text.contains("/tmp/test-session/plan.md"),
                "must name the plan file so the model knows the one editable path: {text}"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn plan_mode_allows_plan_file_edit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_gate_actor().await;
            activate_plan_mode(&actor);
            let result = prepare(
                &actor,
                search_replace_call_at("call_plan_file", "/tmp/test-session/plan.md"),
            )
            .await;
            assert!(
                result.is_ok(),
                "plan-file edit must pass the gate and prepare; got {:?}",
                result.err()
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn inactive_plan_mode_does_not_gate_edits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = build_gate_actor().await;
            let result = prepare(
                &actor,
                search_replace_call_at("call_no_plan", "/tmp/src/main.rs"),
            )
            .await;
            assert!(
                result.is_ok(),
                "edit outside plan mode must prepare; got {:?}",
                result.err()
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn plan_gate_sees_hook_rewritten_path() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = build_gate_actor().await;
            install_pre_tool_use_hooks(
                &mut actor,
                vec![pre_tool_use_spec(
                    "test/pretooluse",
                    None,
                    r#"echo '{"hookSpecificOutput":{"updatedInput":{"file_path":"/tmp/src/main.rs","old_string":"a","new_string":"b"}}}'"#,
                )],
            );
            activate_plan_mode(&actor);
            let result = prepare(
                    &actor,
                    search_replace_call_at("call_hook_gate", "/tmp/test-session/plan.md"),
                )
                .await;
            assert!(
                matches!(result, Err(ToolLoop::Continue)),
                "plan gate must reject the hook-rewritten non-plan path; got {result:?}"
            );
        })
        .await;
}
