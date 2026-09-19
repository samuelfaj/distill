// Modified for Distill by Samuel Fajreldines, 2026.
use distill_tools::implementations::distill::task::types::{
    SubagentCompletionSummary, SubagentSnapshot, SubagentSnapshotStatus,
};
use distill_tools::reminders::task_completion::format_between_turn_completions;

fn summary(
    id: &str,
    typ: &str,
    desc: &str,
    success: bool,
    ms: u64,
    tools: u32,
) -> SubagentCompletionSummary {
    let output = format!("the answer for {id}");
    let status = if success {
        SubagentSnapshotStatus::Completed {
            output: String::new(),
            tool_calls: tools,
            turns: 1,
            worktree_path: None,
        }
    } else {
        SubagentSnapshotStatus::Failed {
            error: "context window exhausted".into(),
        }
    };
    SubagentCompletionSummary {
        snapshot: SubagentSnapshot {
            subagent_id: id.into(),
            description: desc.into(),
            subagent_type: typ.into(),
            status,
            started_at_epoch_ms: 0,
            duration_ms: ms,
            persona: None,
        },
        loop_task_id: None,
        tool_calls: tools,
        full_output_bytes: output.len(),
        output: std::sync::Arc::from(output),
    }
}

#[test]
fn single_successful_completion_with_poll_tool() {
    let completions = vec![summary(
        "abc-123",
        "explore",
        "Search for auth patterns",
        true,
        12300,
        5,
    )];
    let result = format_between_turn_completions(&completions, Some("get_task_output"), None, None);
    assert!(result.starts_with("While you were idle, 1 background subagent completed:\n"));
    assert!(result.contains("[explore]"));
    assert!(result.contains("completed successfully"));
    assert!(result.contains("12.3s"));
    assert!(result.contains("5 tool calls"));
    assert!(result.contains("abc-123"));
    assert!(
        result.contains(
            "(12.3s, 5 tool calls)\n\
             === Task abc-123 ===\n\
             Command: [subagent:explore] Search for auth patterns\n\
             Status: completed\n\
             Duration: 12.30s\n\
             Exit Code: 0\n\
             \n\
             === Output ===\n\
             the answer for abc-123\n\
             \n\
             <subagent_meta>id=abc-123, "
        ),
        "{result}"
    );
    assert!(result.contains("\n</subagent_result>\n"), "{result}");
    assert!(!result.contains("to see the full output"), "{result}");
}

#[test]
fn scheduled_completion_includes_resolved_cleanup_tool() {
    let mut completion = summary("abc-123", "explore", "Monitor work", true, 12300, 5);
    completion.loop_task_id = Some("loop-123".into());

    let result = format_between_turn_completions(
        &[completion],
        Some("get_task_output"),
        Some("renamed_scheduler_delete"),
        Some("renamed_scheduler_create"),
    );

    assert!(result.contains(
        "Check the subagent output using get_task_output(\"abc-123\"). If there are issues, proactively debug and fix them, do not just report it to the user."
    ));
    assert!(result.contains("renamed_scheduler_delete(\"loop-123\")"));
    assert!(result.contains("renamed_scheduler_create(new_prompt, interval, \"loop-123\")"));
    assert!(!result.contains("update it with scheduler_create("));
}

#[test]
fn failed_completion_with_poll_tool() {
    let completion = summary(
        "def-456",
        "general-purpose",
        "Implement feature X",
        false,
        45200,
        12,
    );
    let result =
        format_between_turn_completions(&[completion], Some("get_task_output"), None, None);
    assert!(result.contains("failed"));
    assert!(result.contains("45.2s"));
    assert!(result.contains("12 tool calls"));
    assert!(
        result.contains(
            "\n=== Task def-456 ===\n\
             Command: [subagent:general-purpose] Implement feature X\n\
             Status: failed\n\
             Duration: 45.20s\n\
             Exit Code: 1\n\
             \n\
             === Output ===\n\
             context window exhausted\n"
        ),
        "{result}"
    );
    assert!(!result.contains("subagent_meta"), "{result}");
}

#[test]
fn multiple_completions_batched_with_poll_tool() {
    let completions = vec![
        summary("a", "explore", "task 1", true, 1000, 2),
        summary("b", "general-purpose", "task 2", false, 5000, 8),
        summary("c", "explore", "task 3", true, 3000, 4),
    ];
    let result = format_between_turn_completions(&completions, Some("get_task_output"), None, None);
    assert!(result.starts_with("While you were idle, 3 background subagents completed:\n"));
    // All three entries appear
    let b = "\n\n- [general-purpose] \"task 2\" \u{2014} failed (5.0s, 8 tool calls)\n";
    let c = "\n\n- [explore] \"task 3\" \u{2014} completed successfully (3.0s, 4 tool calls)\n";
    assert!(result.contains("\n=== Task a ===\n"), "{result}");
    assert!(result.contains(b), "{result}");
    assert!(result.contains("\n=== Task b ===\n"), "{result}");
    assert!(result.contains(c), "{result}");
    assert!(result.contains("\n=== Task c ===\n"), "{result}");
}

#[test]
fn no_poll_tool_inlines_output() {
    // No BackgroundTaskAction tool is exposed
    // The model has no way to retrieve the subagent's output later, so the completion notification MUST inline the output text
    let completions = vec![summary(
        "abc-123",
        "explore",
        "Search for auth patterns",
        true,
        12300,
        5,
    )];
    let result = format_between_turn_completions(&completions, None, None, None);
    assert!(result.contains("[explore]"));
    assert!(result.contains("abc-123"));
    assert!(
        !result.contains("get_task_output"),
        "must not mention a polling tool when none is available: {result}"
    );
    assert!(
        result.contains("\n=== Output ===\nthe answer for abc-123\n\n<subagent_meta>"),
        "must inline the subagent's output text: {result}"
    );
}
