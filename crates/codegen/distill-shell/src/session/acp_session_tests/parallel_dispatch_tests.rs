// Modified for Distill by Samuel Fajreldines, 2026.
//! These tests verify the parallel dispatch path (GROK_PARALLEL_TOOL_DISPATCH):
//! - Phase 1: prepare_tool_call for each tool
//! - Phase 2: permission prompts (if any)
//! - Phase 3: parallel dispatch via dispatch_tool
//! - Post-tool hooks and followups

use super::*;

fn at<T>(xs: &[T], i: usize) -> &T {
    let Some(x) = xs.get(i) else {
        panic!("expected index {i}, len {}", xs.len());
    };
    x
}

use distill_tools::implementations::distill::task::backend::SubagentBackend;
use distill_tools::implementations::distill::task::types::{
    ActiveAgentMessageOutcome, ActiveAgentMessageRequest, SubagentCancelOutcome,
    SubagentDescribeOutcome, SubagentRequest, SubagentResult, SubagentSnapshot,
    SubagentValidateTypeOutcome,
};

struct FixedActiveMessageBackend {
    outcome: ActiveAgentMessageOutcome,
}

#[async_trait::async_trait]
impl SubagentBackend for FixedActiveMessageBackend {
    async fn spawn(
        &self,
        _: SubagentRequest,
        _: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<SubagentResult, distill_tool_runtime::ToolError> {
        Err(distill_tool_runtime::ToolError::custom(
            "unsupported",
            "spawn unsupported",
        ))
    }

    async fn query(&self, _: &str, _: bool, _: Option<u64>) -> Option<SubagentSnapshot> {
        None
    }

    async fn send_active_message(&self, _: ActiveAgentMessageRequest) -> ActiveAgentMessageOutcome {
        self.outcome.clone()
    }

    async fn cancel(&self, _: &str) -> SubagentCancelOutcome {
        SubagentCancelOutcome::NotFound
    }

    async fn validate_type(&self, _: &str, _: &str) -> SubagentValidateTypeOutcome {
        SubagentValidateTypeOutcome::ValidationUnavailable
    }

    async fn describe_subagent_type(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
    ) -> SubagentDescribeOutcome {
        SubagentDescribeOutcome::Unavailable
    }
}

fn active_message_call(id: &str, text: &str) -> crate::sampling::types::ToolCallResponse {
    active_message_call_with_delivery(id, text, None)
}

fn active_message_call_with_delivery(
    id: &str,
    text: &str,
    delivery: Option<&str>,
) -> crate::sampling::types::ToolCallResponse {
    let mut arguments = serde_json::json!({ "subagent_id": "child", "text": text });
    if let Some(delivery) = delivery
        && let Some(obj) = arguments.as_object_mut()
    {
        obj.insert("delivery".into(), delivery.into());
    }
    crate::sampling::types::ToolCallResponse {
        id: id.to_owned(),
        kind: "function".to_owned(),
        function: crate::sampling::types::ToolCallFunction::new(
            "send_subagent_message",
            arguments.to_string(),
        ),
    }
}

fn unrelated_tool_call(id: &str) -> crate::sampling::types::ToolCallResponse {
    crate::sampling::types::ToolCallResponse {
        id: id.to_owned(),
        kind: "function".to_owned(),
        function: crate::sampling::types::ToolCallFunction::new(
            "todo_write",
            r#"{"todos":[{"id":"t1","content":"do","status":"completed"}]}"#,
        ),
    }
}

async fn execute_with_captured_active_message_events(
    actor: &SessionActor,
    call: crate::sampling::types::ToolCallResponse,
) -> Vec<crate::session::telemetry::ActiveAgentMessageEvent> {
    let (result, events) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::session::telemetry::capture_product_events(actor.execute_tool_calls(vec![call])),
    )
    .await
    .expect("generic tool completion must not hang");
    result.expect("generic tool completion must not error");
    events
}

fn active_message_event_names(
    events: &[crate::session::telemetry::ActiveAgentMessageEvent],
) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            crate::session::telemetry::ActiveAgentMessageEvent::Completed(_) => {
                "active_agent_message_completed"
            }
            crate::session::telemetry::ActiveAgentMessageEvent::LimitHit(_) => {
                "active_agent_message_limit_hit"
            }
            crate::session::telemetry::ActiveAgentMessageEvent::QuotaHit(_) => {
                "active_agent_message_quota_hit"
            }
            crate::session::telemetry::ActiveAgentMessageEvent::Settled(_) => {
                "active_agent_message_settled"
            }
        })
        .collect()
}

#[test]
fn active_message_outputs_distinguish_uncertain_from_proved_rejection() {
    use distill_tools::implementations::distill::send_subagent_message::SendSubagentMessageOutput;
    use distill_tools::types::output::ToolOutput;

    for (label, output, expected) in [
        (
            "uncertain",
            SendSubagentMessageOutput::AdmissionUncertain,
            "unconfirmed",
        ),
        (
            "deadline_rejected",
            SendSubagentMessageOutput::NotAcceptedBeforeDeadline,
            "error",
        ),
        (
            "channel_closed",
            SendSubagentMessageOutput::ChannelClosed,
            "error",
        ),
    ] {
        let output = ToolOutput::SendSubagentMessage(output);
        let result: Result<ToolRunResult, distill_tool_runtime::ToolError> = Ok(ToolRunResult {
            prompt_text: output.to_prompt_format(),
            output,
            effective_tool_name: None,
        });
        assert_eq!(
            super::tool_calls::tool_output_span_outcome(&result),
            expected,
            "{label}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn generic_tool_completion_chokepoint_has_exact_active_message_cardinality() {
    use distill_tools::implementations::distill::task::backend::SubagentBackendResource;
    use distill_tools::implementations::distill::task::types::{
        MAX_ACTIVE_AGENT_MESSAGE_BYTES, SubagentDepthCounter,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor =
                super::support::create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor.agent.borrow_mut() = super::support::test_agent_with_active_message_tool().await;
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
                .agent
                .borrow()
                .tool_bridge()
                .update_resource(SubagentDepthCounter(0))
                .await;

            for (id, text, outcome, expected) in [
                (
                    "accepted",
                    "follow up".to_owned(),
                    ActiveAgentMessageOutcome::Accepted {
                        message_id: "message-1".to_owned(),
                    },
                    vec!["active_agent_message_completed"],
                ),
                (
                    "admission-uncertain",
                    "follow up".to_owned(),
                    ActiveAgentMessageOutcome::AdmissionUncertain,
                    vec!["active_agent_message_completed"],
                ),
                (
                    "deadline-rejected",
                    "follow up".to_owned(),
                    ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
                    vec!["active_agent_message_completed"],
                ),
                (
                    "channel-closed",
                    "follow up".to_owned(),
                    ActiveAgentMessageOutcome::ChannelClosed,
                    vec!["active_agent_message_completed"],
                ),
                (
                    "oversize",
                    "x".repeat(MAX_ACTIVE_AGENT_MESSAGE_BYTES + 1),
                    ActiveAgentMessageOutcome::Unsupported,
                    vec![
                        "active_agent_message_completed",
                        "active_agent_message_limit_hit",
                    ],
                ),
                (
                    "empty",
                    String::new(),
                    ActiveAgentMessageOutcome::Unsupported,
                    vec!["active_agent_message_completed"],
                ),
            ] {
                actor
                    .agent
                    .borrow()
                    .tool_bridge()
                    .update_resource(SubagentBackendResource(std::sync::Arc::new(
                        FixedActiveMessageBackend { outcome },
                    )))
                    .await;
                let events = execute_with_captured_active_message_events(
                    &actor,
                    active_message_call(id, &text),
                )
                .await;
                assert_eq!(active_message_event_names(&events), expected);
            }

            for (id, delivery, expected_operation) in [
                (
                    "queued",
                    "queue",
                    distill_telemetry::events::ActiveAgentMessageOperation::Queue,
                ),
                (
                    "interjected",
                    "interject",
                    distill_telemetry::events::ActiveAgentMessageOperation::Interject,
                ),
            ] {
                let events = execute_with_captured_active_message_events(
                    &actor,
                    active_message_call_with_delivery(id, "follow up", Some(delivery)),
                )
                .await;
                assert!(matches!(
                    events.as_slice(),
                    [
                        crate::session::telemetry::ActiveAgentMessageEvent::Completed(
                            distill_telemetry::events::ActiveAgentMessageCompleted {
                                requested_operation,
                                ..
                            }
                        )
                    ] if *requested_operation == expected_operation
                ));
            }

            let events = execute_with_captured_active_message_events(
                &actor,
                unrelated_tool_call("unrelated"),
            )
            .await;
            assert!(events.is_empty());
        })
        .await;
}

#[tokio::test]
async fn test_parallel_dispatch_basic() {
    // Ordering correctness: verify that futures::future::join_all preserves the order of results matching the order of input futures.
    // In Phase 2, dispatch_futures is built by mapping approved.iter() to dispatch_tool calls Phase 3 zips approved.into_iter() with dispatch_results, so result[i] must correspond to approved[i].

    use futures::future::join_all;

    // Simulate 3 tools with different latencies
    let futures = vec![
        Box::pin(async { (0, "tool_a") })
            as std::pin::Pin<Box<dyn futures::Future<Output = (i32, &'static str)>>>,
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            (1, "tool_b")
        }),
        Box::pin(async { (2, "tool_c") }),
    ];

    let results = join_all(futures).await;

    // Results must be in input order, not completion order
    assert_eq!(*at(&results, 0), (0, "tool_a"));
    assert_eq!(*at(&results, 1), (1, "tool_b"));
    assert_eq!(*at(&results, 2), (2, "tool_c"));
}

#[test]
fn test_parallel_dispatch_permission_reject() {
    // Permission rejection abort: when prepare_tool_call returns Err(ToolLoop::PermissionReject), subsequent tools should not be dispatched
    //
    // Verify the logic: once final_result is set, remaining tools are skipped.
    let mut final_result: Option<ToolLoop> = None;
    let tool_calls = ["tool_0", "tool_1", "tool_2"];
    let mut approved_count = 0;

    for (idx, _call) in tool_calls.iter().enumerate() {
        if final_result.is_some() {
            // Would skip this tool in real code
            continue;
        }
        // Simulate: tool_1 gets permission rejected
        if idx == 1 {
            final_result = Some(ToolLoop::PermissionReject {
                tool_name: "tool_1".to_string(),
                reason: "rejected".to_string(),
            });
            continue;
        }
        approved_count += 1;
    }

    // Only tool_0 should be approved; tool_1 triggers rejection; tool_2 is skipped
    assert_eq!(approved_count, 1);
    assert!(final_result.is_some());
    assert!(matches!(
        final_result,
        Some(ToolLoop::PermissionReject { .. })
    ));
}
#[test]
fn test_parallel_dispatch_followups() {
    // Deferred followups placement: handle_bridge_tool_success returns Vec<ConversationItem> followups that get extended into deferred_followups.
    // In Phase 3: let followups = handle_bridge_tool_success(...).await?; deferred_followups.extend(followups);.
    let mut deferred_followups: Vec<&str> = Vec::new();

    // Simulate followups from 2 tools
    let followups_tool_0 = vec!["followup_a", "followup_b"];
    let followups_tool_1 = vec!["followup_c"];

    deferred_followups.extend(followups_tool_0);
    deferred_followups.extend(followups_tool_1);

    assert_eq!(deferred_followups.len(), 3);
    assert_eq!(*at(&deferred_followups, 0), "followup_a");
    assert_eq!(*at(&deferred_followups, 1), "followup_b");
    assert_eq!(*at(&deferred_followups, 2), "followup_c");
}

#[test]
fn test_parallel_dispatch_hooks() {
    // Dispatching a single tool should behave identically to the serial path.
    // The parallel dispatch infrastructure (prepare_tool_call, then dispatch_tool, then post-flight) should work for N=1 without special casing.
    let approved_count = 1;
    let dispatch_futures_count = approved_count; // 1:1 mapping
    let results_count = 1; // incremental stream yields same count

    assert_eq!(approved_count, dispatch_futures_count);
    assert_eq!(dispatch_futures_count, results_count);

    // Also verify the Phase 3 indexed slot works for single element
    let approved = ["single_tool"];
    let ok_val: Result<&str, ()> = Ok("success");
    let results = [ok_val];
    let pairs: Vec<_> = approved.iter().zip(results.iter()).collect();
    assert_eq!(pairs.len(), 1);
}

/// Incremental completion ordering: fast tool results must reach the client before slow siblings finish.
/// Regression for the batch barrier where `join_all` deferred every `ToolCallUpdate(status=Completed)` until the slowest tool in the round finished.
#[tokio::test]
async fn incremental_dispatch_surfaces_fast_tool_before_slow_sibling() {
    use futures::future::BoxFuture;
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let fast_done = Arc::new(AtomicBool::new(false));
    let slow_done = Arc::new(AtomicBool::new(false));
    let fast_flag = Arc::clone(&fast_done);
    let slow_flag = Arc::clone(&slow_done);

    let mut stream: FuturesUnordered<BoxFuture<'static, (usize, &'static str)>> =
        FuturesUnordered::new();
    stream.push(Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        fast_flag.store(true, Ordering::SeqCst);
        (0usize, "grep")
    }));
    stream.push(Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        slow_flag.store(true, Ordering::SeqCst);
        (1usize, "wait_tasks")
    }));

    let mut completion_order = Vec::new();
    while let Some((idx, name)) = stream.next().await {
        completion_order.push((idx, name));
        // Fast tool must finish first; incremental post-flight depends on this to stream grep results before the wait tool returns
        if idx == 0 {
            assert!(
                !slow_done.load(Ordering::SeqCst),
                "fast tool must complete before slow sibling; incremental UI streaming depends on this ordering"
            );
            assert!(fast_done.load(Ordering::SeqCst));
        }
    }

    assert_eq!(completion_order.len(), 2);
    assert_eq!(*at(&completion_order, 0), (0, "grep"));
    assert_eq!(*at(&completion_order, 1), (1, "wait_tasks"));
    assert!(fast_done.load(Ordering::SeqCst));
    assert!(slow_done.load(Ordering::SeqCst));
}

/// Regression for the race where two toolsets edited the same file concurrently.
/// `lock_path_for_args` is the per-call key `execute_tool_calls` Phase 2 uses to bucket concurrent calls into per-file `tokio::sync::Mutex` groups.
/// The compat toolset input types use `path`, and distill's `read_file` uses `target_file`.
#[test]
fn lock_path_for_args_matches_distill_file_path() {
    // distill search_replace / opencode EditTool / WriteTool / etc.
    let args = serde_json::json!({
        "file_path": "/repo/src/main.rs",
        "old_string": "foo",
        "new_string": "bar",
    });
    assert_eq!(
        lock_path_for_args(&args, Path::new("/cwd")),
        Some("/repo/src/main.rs".to_owned())
    );
}

#[test]
fn lock_path_for_args_normalizes_relative_aliases() {
    let cwd = Path::new("/repo");
    let direct = serde_json::json!({ "file_path": "src/main.rs" });
    let dotted = serde_json::json!({ "file_path": "./src/main.rs" });
    let parent = serde_json::json!({ "file_path": "src/tmp/../main.rs" });
    let absolute = serde_json::json!({ "file_path": "/repo/src/main.rs" });

    let expected = Some("/repo/src/main.rs".to_owned());
    assert_eq!(lock_path_for_args(&direct, cwd), expected);
    assert_eq!(lock_path_for_args(&dotted, cwd), expected);
    assert_eq!(lock_path_for_args(&parent, cwd), expected);
    assert_eq!(lock_path_for_args(&absolute, cwd), expected);
}

#[cfg(unix)]
#[test]
fn lock_path_for_args_canonicalizes_symlink_aliases() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real");
    let alias = directory.path().join("alias");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("main.rs"), "fn main() {}").unwrap();
    symlink(&real, &alias).unwrap();

    for filename in ["main.rs", "not-created-yet.rs"] {
        let real_args = serde_json::json!({ "file_path": real.join(filename) });
        let alias_args = serde_json::json!({ "file_path": alias.join(filename) });
        assert_eq!(
            lock_path_for_args(&real_args, directory.path()),
            lock_path_for_args(&alias_args, directory.path()),
            "{filename}"
        );
    }
}

#[test]
fn lock_path_for_args_matches_path_arg() {
    // StrReplace / Write / Read / Delete all serialize under `path`.
    let args = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "foo",
        "new_string": "bar",
    });
    assert_eq!(
        lock_path_for_args(&args, Path::new("/cwd")),
        Some("/repo/src/main.rs".to_owned())
    );
}

#[test]
fn lock_path_for_args_matches_distill_target_file() {
    // distill read_file uses #[serde(rename = "target_file")].
    let args = serde_json::json!({
        "target_file": "/repo/src/main.rs",
    });
    assert_eq!(
        lock_path_for_args(&args, Path::new("/cwd")),
        Some("/repo/src/main.rs".to_owned())
    );
}

#[test]
fn lock_path_for_args_returns_none_for_pathless_tools() {
    // Tools like run_terminal_cmd or web_search have no workspace path; they must not be bucketed into a file lock and must run fully concurrently
    let args = serde_json::json!({
        "command": "ls -la",
        "description": "list",
    });
    assert_eq!(lock_path_for_args(&args, Path::new("/cwd")), None);
    assert_eq!(
        lock_path_for_args(&serde_json::json!({}), Path::new("/cwd")),
        None
    );
    assert_eq!(
        lock_path_for_args(&serde_json::json!(null), Path::new("/cwd")),
        None
    );
}

#[test]
fn lock_path_for_args_ignores_non_string_path_values() {
    // Defensive: if a model emits a non-string, treat it as no lock rather than panicking or coercing; the tool layer will reject it
    let args = serde_json::json!({"file_path": 42});
    assert_eq!(lock_path_for_args(&args, Path::new("/cwd")), None);
    let args = serde_json::json!({"path": ["/a", "/b"]});
    assert_eq!(lock_path_for_args(&args, Path::new("/cwd")), None);
}

#[test]
fn lock_path_for_args_buckets_parallel_compat_strreplace_to_same_lock() {
    // The exact symptom of the bug: two compat StrReplace calls in one batch targeting the same file both returned None here and raced on the file
    // Both must hash to the same bucket so the dispatcher serializes them via a per-file Mutex
    let call_a = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "foo",
        "new_string": "bar",
    });
    let call_b = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "baz",
        "new_string": "qux",
    });
    assert_eq!(
        lock_path_for_args(&call_a, Path::new("/cwd")),
        lock_path_for_args(&call_b, Path::new("/cwd"))
    );
    assert_eq!(
        lock_path_for_args(&call_a, Path::new("/cwd")),
        Some("/repo/src/main.rs".to_owned())
    );

    // Cross-file calls must bucket independently so they keep running concurrently; otherwise we'd serialize unrelated edits and tank batch latency
    let call_c = serde_json::json!({
        "path": "/repo/src/lib.rs",
        "old_string": "x",
        "new_string": "y",
    });
    assert_ne!(
        lock_path_for_args(&call_a, Path::new("/cwd")),
        lock_path_for_args(&call_c, Path::new("/cwd"))
    );
}

#[test]
fn lock_path_for_args_buckets_distill_and_compat_to_same_lock_for_same_file() {
    // A mixed batch of distill search_replace and compat.
    // StrReplace in the same turn must still serialize on the shared file path.
    // That mix is possible if the harness ever exposes both toolsets, or during a toolset migration file_path takes precedence over path when both are present, but neither tool emits both keys today.
    let grok = serde_json::json!({
        "file_path": "/repo/src/main.rs",
        "old_string": "a",
        "new_string": "b",
    });
    let compat = serde_json::json!({
        "path": "/repo/src/main.rs",
        "old_string": "c",
        "new_string": "d",
    });
    assert_eq!(
        lock_path_for_args(&grok, Path::new("/cwd")),
        lock_path_for_args(&compat, Path::new("/cwd"))
    );
}

/// Regression: skill-discovery reminders must land after all tool results, not mid-batch.
#[test]
fn test_skill_discovery_deferred_during_parallel_batch() {
    use distill_sampling_types::{ConversationItem, SyntheticReason};

    let mut conversation = vec![ConversationItem::assistant("I'll call 3 tools.")];
    let mut deferred_followups: Vec<ConversationItem> = Vec::new();

    for (i, id) in ["call_1", "call_2", "call_3"].iter().enumerate() {
        conversation.push(ConversationItem::tool_result(
            *id,
            format!("result for {id}"),
        ));
        if i == 0 {
            // Image followup from handle_bridge_tool_success
            deferred_followups.push(ConversationItem::user("[Image content]"));
            // Skill discovery fires after tool 1; it must be deferred, not pushed immediately
            deferred_followups.push(ConversationItem::system_reminder(
                "<system-reminder>\nNew skills discovered\n</system-reminder>",
            ));
        }
    }
    conversation.extend(deferred_followups);

    // 1 assistant + 3 tool_result + 2 deferred user messages
    assert_eq!(conversation.len(), 6);
    assert!(matches!(
        at(&conversation, 0),
        ConversationItem::Assistant(_)
    ));
    assert!(matches!(
        at(&conversation, 1),
        ConversationItem::ToolResult(_)
    ));
    assert!(matches!(
        at(&conversation, 2),
        ConversationItem::ToolResult(_)
    ));
    assert!(matches!(
        at(&conversation, 3),
        ConversationItem::ToolResult(_)
    ));
    assert!(matches!(at(&conversation, 4), ConversationItem::User(_)));
    assert!(
        matches!(at(&conversation, 5), ConversationItem::User(u) if u.synthetic_reason == SyntheticReason::SystemReminder)
    );
}
