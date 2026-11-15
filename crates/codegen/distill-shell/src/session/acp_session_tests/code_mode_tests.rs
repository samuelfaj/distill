//! Code Mode: a script's tool calls run through the session's own tool-call path.
//!
//! What must hold: a call made from a script is gated exactly like a call made by the model (permission prompts,
//! hooks), and only the `code` call itself is answered in the conversation.
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender;
use distill_tools::implementations::distill::CodeTool;
use distill_tools::implementations::distill::code_mode::{ScriptToolCall, ScriptToolCallSender};
use distill_tools::registry::types::ToolConfig;

use super::support::*;
use super::tool_calls::script_tool_calls::ScriptCalls;
use super::*;

type GatewayTx = tokio::sync::mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>;
type GatewayRx = tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>;

/// An actor with read, edit and `code` tools and Code Mode wired as a session spawn wires it.
async fn code_mode_actor(
    extra_tools: Vec<ToolConfig>,
    hooks: Vec<distill_hooks::config::HookSpec>,
    yolo: bool,
    gateway_tx: GatewayTx,
) -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedSender<ScriptToolCall>,
) {
    let hook_gateway = AcpAgentGatewaySender::new(gateway_tx.clone());
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    let mut tools = read_and_edit_toolset();
    tools.push(ToolConfig::for_tool::<CodeTool>());
    tools.extend(extra_tools);
    *actor.agent.borrow_mut() = test_agent_with_tools(tools).await;
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
    let (script_tx, script_rx) = tokio::sync::mpsc::unbounded_channel();
    actor
        .agent
        .borrow()
        .tool_bridge()
        .update_resource(ScriptToolCallSender(script_tx.clone()))
        .await;
    actor.script_calls = ScriptCalls::new(Some(script_rx));
    install_permission_manager(&mut actor, yolo, hook_gateway);
    install_pre_tool_use_hooks(&mut actor, hooks);
    (actor, script_tx)
}

fn code_call(id: &str, code: &str) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_owned(),
        kind: "function".to_owned(),
        function: crate::sampling::types::ToolCallFunction::new(
            "code",
            serde_json::json!({ "code": code }).to_string(),
        ),
    }
}

/// Answers every permission prompt with `option` and records which tool call each prompt was for.
fn answer_prompts(
    mut gateway_rx: GatewayRx,
    option: &'static str,
) -> Arc<std::sync::Mutex<Vec<String>>> {
    let prompted: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = prompted.clone();
    tokio::task::spawn_local(async move {
        while let Some(message) = gateway_rx.recv().await {
            match message {
                distill_acp_lib::AcpClientMessage::RequestPermission(args) => {
                    seen.lock()
                        .unwrap()
                        .push(args.request.tool_call.tool_call_id.to_string());
                    let _ = args
                        .response_tx
                        .send(Ok(acp::RequestPermissionResponse::new(
                            acp::RequestPermissionOutcome::Selected(
                                acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(
                                    option,
                                )),
                            ),
                        )));
                }
                distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                _ => {}
            }
        }
    });
    prompted
}

async fn run(actor: &SessionActor, call: ToolCallResponse) -> ToolLoop {
    tokio::time::timeout(
        Duration::from_secs(30),
        actor.execute_tool_calls(vec![call]),
    )
    .await
    .expect("execute_tool_calls must not hang")
    .expect("execute_tool_calls must not error")
}

async fn tool_results(actor: &SessionActor) -> Vec<String> {
    actor
        .chat_state_handle
        .get_conversation()
        .await
        .iter()
        .filter_map(|item| match item {
            distill_sampling_types::ConversationItem::ToolResult(result) => {
                Some(result.tool_call_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// A file a script edits, and the `tools.search_replace` call that turns its `before` into `after`.
fn editable_file(dir: &tempfile::TempDir) -> (std::path::PathBuf, String) {
    let path = dir.path().join("target.txt");
    std::fs::write(&path, "before\n").expect("write target");
    let edit = format!(
        "await tools.search_replace({{ file_path: {path:?}, old_string: 'before', new_string: 'after' }})"
    );
    (path, edit)
}

#[tokio::test(flavor = "current_thread")]
async fn a_scripts_read_runs_and_only_the_scripts_answer_enters_the_conversation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (actor, _script_tx) =
                code_mode_actor(vec![], vec![], /*yolo=*/ true, gateway_tx).await;
            spawn_gateway_loop(gateway_rx);
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("notes.txt");
            std::fs::write(&path, "alpha\nneedle here\nomega\n").expect("write notes");

            let outcome = run(
                &actor,
                code_call(
                    "call_code",
                    &format!(
                        "const text = await tools.read_file({{ target_file: {path:?} }});\n\
                         return text.includes('needle here') ? 'found' : 'missing: ' + text;"
                    ),
                ),
            )
            .await;

            assert!(matches!(outcome, ToolLoop::Continue), "{outcome:?}");
            assert_eq!(tool_result_text(&actor, "call_code").await, "found");
            // The read's own result went to the script. In the conversation it would answer a tool call the
            // model never made, which providers reject.
            assert_eq!(tool_results(&actor).await, ["call_code"]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_scripts_edit_is_prompted_like_a_direct_edit_and_runs_once_approved() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (actor, _script_tx) =
                code_mode_actor(vec![], vec![], /*yolo=*/ false, gateway_tx).await;
            let prompted = answer_prompts(gateway_rx, "allow-once");
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, edit) = editable_file(&dir);

            let outcome = run(
                &actor,
                code_call("call_code", &format!("{edit};\nreturn 'edited';")),
            )
            .await;

            assert!(matches!(outcome, ToolLoop::Continue), "{outcome:?}");
            let prompted = prompted.lock().unwrap().clone();
            assert_eq!(
                prompted.len(),
                1,
                "the script itself is not prompted, its edit is: {prompted:?}"
            );
            assert!(prompted[0].starts_with("call_code.script-"), "{prompted:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
            assert_eq!(tool_result_text(&actor, "call_code").await, "edited");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejecting_a_scripts_edit_stops_the_edit_and_the_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (actor, _script_tx) =
                code_mode_actor(vec![], vec![], /*yolo=*/ false, gateway_tx).await;
            let prompted = answer_prompts(gateway_rx, "reject-once");
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, edit) = editable_file(&dir);

            let outcome = run(&actor, code_call("call_code", &format!("{edit};\nreturn 'edited';"))).await;

            // A "No" at a prompt ends the tool loop for a direct call; behind a script it must do the same.
            assert!(
                matches!(&outcome, ToolLoop::PermissionReject { tool_name, .. } if tool_name == "search_replace"),
                "{outcome:?}"
            );
            assert_eq!(prompted.lock().unwrap().len(), 1);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
            // The `code` call is still answered, so the model's tool call does not dangle.
            let answer = tool_result_text(&actor, "call_code").await;
            assert!(answer.contains("Script failed"), "{answer}");
            assert_eq!(tool_results(&actor).await, ["call_code"]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_pre_tool_use_hook_blocks_a_scripts_call() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let deny_edits = pre_tool_use_spec(
                "test/no-edits",
                Some("search_replace"),
                r#"echo '{"decision":"deny","reason":"edits are frozen"}'; exit 2"#,
            );
            let (actor, _script_tx) =
                code_mode_actor(vec![], vec![deny_edits], /*yolo=*/ true, gateway_tx).await;
            spawn_gateway_loop(gateway_rx);
            let dir = tempfile::tempdir().expect("tempdir");
            let (path, edit) = editable_file(&dir);

            run(
                &actor,
                code_call(
                    "call_code",
                    &format!("try {{ {edit}; return 'edited'; }} catch (error) {{ return 'blocked: ' + error.message; }}"),
                ),
            )
            .await;

            // Always-approve mode skips prompts, never hooks: a hook is the user's own rule.
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
            let answer = tool_result_text(&actor, "call_code").await;
            assert!(
                answer.starts_with("blocked: ") && answer.contains("edits are frozen"),
                "{answer}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_script_cannot_reach_tools_that_steer_the_session() {
    use distill_tools::implementations::distill::todo::TodoWriteTool;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (actor, _script_tx) = code_mode_actor(
                vec![ToolConfig::for_tool::<TodoWriteTool>()],
                vec![],
                /*yolo=*/ true,
                gateway_tx,
            )
            .await;
            spawn_gateway_loop(gateway_rx);

            run(
                &actor,
                code_call(
                    "call_code",
                    "const refusals = [];\n\
                     for (const name of ['todo_write', 'code', 'no_such_tool']) {\n\
                       try { await tools[name]({}); refusals.push(name + ' ran'); }\n\
                       catch (error) { refusals.push(error.message); }\n\
                     }\n\
                     return refusals.join('\\n');",
                ),
            )
            .await;

            let answer = tool_result_text(&actor, "call_code").await;
            let refusals: Vec<&str> = answer.lines().collect();
            assert_eq!(
                refusals,
                [
                    "`todo_write` cannot be called from a script; call it directly. A script can call: read_file, search_replace.",
                    "`code` cannot be called from a script; call it directly. A script can call: read_file, search_replace.",
                    "there is no tool named `no_such_tool`. A script can call: read_file, search_replace.",
                ]
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_left_behind_by_a_cancelled_script_never_runs() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (actor, script_tx) =
                code_mode_actor(vec![], vec![], /*yolo=*/ true, gateway_tx).await;
            spawn_gateway_loop(gateway_rx);
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("target.txt");
            std::fs::write(&path, "before\n").expect("write target");

            // A turn was cancelled while its script waited on this edit: the request is still queued, and
            // nobody is listening for the answer.
            let (reply, listener) = tokio::sync::oneshot::channel();
            drop(listener);
            script_tx
                .send(ScriptToolCall {
                    parent_call_id: "call_cancelled".to_owned(),
                    tool: "search_replace".to_owned(),
                    args: serde_json::json!({
                        "file_path": path, "old_string": "before", "new_string": "after"
                    }),
                    reply,
                })
                .expect("queue the stale call");

            // The next turn serves the queue; its own script's call is answered, the stale edit is dropped.
            run(
                &actor,
                code_call(
                    "call_code",
                    &format!("return await tools.read_file({{ target_file: {path:?} }});"),
                ),
            )
            .await;

            assert!(
                tool_result_text(&actor, "call_code")
                    .await
                    .contains("before")
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
        })
        .await;
}

#[test]
fn the_card_title_shows_what_the_script_starts_with() {
    use super::tool_calls::script_tool_calls::script_call_title;
    assert_eq!(
        script_call_title(
            &serde_json::json!({ "code": "\n  // rename retryPolicy\nconst a = 1;" })
        ),
        "Code: // rename retryPolicy"
    );
    let long = format!("const names = [{}];", "'x', ".repeat(40));
    let title = script_call_title(&serde_json::json!({ "code": long }));
    assert!(
        title.ends_with('…') && title.chars().count() == "Code: ".len() + 72 + 1,
        "{title}"
    );
    assert_eq!(script_call_title(&serde_json::json!({})), "Code");
    assert_eq!(
        script_call_title(
            &serde_json::json!({ "code": "const a = 1;", "description": "Count the TODOs" })
        ),
        "Code: Count the TODOs"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_code_call_itself_raises_no_prompt() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (actor, _script_tx) =
                code_mode_actor(vec![], vec![], /*yolo=*/ false, gateway_tx).await;
            let prompted = answer_prompts(gateway_rx, "reject-once");

            // Pure computation touches nothing, so there is nothing to approve.
            run(
                &actor,
                code_call("call_code", "return [1, 2, 3].map((n) => n * 2).join(',');"),
            )
            .await;

            assert!(prompted.lock().unwrap().is_empty());
            assert_eq!(tool_result_text(&actor, "call_code").await, "2,4,6");
        })
        .await;
}
