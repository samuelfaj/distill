// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::tools::tool_context::BlockingWaitState;
use distill_interjection_core::PendingInterjection;
use distill_tool_types::TaskOutputOutput;
use distill_tools::types::output::ToolOutput;

fn ids(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn interrupted_wait_result_is_cancelled_not_error() {
    let r = interrupted_wait_tool_result(
        &serde_json::json!({
            "task_ids": ["bg-9"],
            "timeout_ms": 60_000
        }),
        WaitInterruptCause::HumanInterjection,
    );
    assert!(r.prompt_text.contains(WAIT_INTERRUPTED_HEAD));
    match &r.output {
        ToolOutput::TaskOutput(TaskOutputOutput::Result(res)) => {
            assert_eq!(res.task_id, "bg-9");
            assert_eq!(res.status, "cancelled");
        }
        other => panic!("expected TaskOutput Result, got {other:?}"),
    }
    assert!(!r.output.is_error());
}

#[test]
fn interrupted_wait_result_does_not_list_ids() {
    let r = interrupted_wait_tool_result(
        &serde_json::json!({
            "task_ids": ["wait-a", "wait-b", "wait-c"],
            "timeout_ms": 600_000
        }),
        WaitInterruptCause::HumanInterjection,
    );
    for id in ["wait-a", "wait-b", "wait-c"] {
        assert!(
            !r.prompt_text.contains(id),
            "interrupt result must not list {id}, got:\n{}",
            r.prompt_text
        );
    }
    let lower = r.prompt_text.to_ascii_lowercase();
    assert!(
        lower.contains("resume") && lower.contains("task_ids"),
        "interrupt result must tell the model to resume the called task_ids, got:\n{}",
        r.prompt_text
    );
    match &r.output {
        ToolOutput::TaskOutput(TaskOutputOutput::Result(res)) => {
            assert_eq!(res.task_id, "wait-a");
            assert_eq!(res.status, "cancelled");
        }
        other => panic!("expected TaskOutput Result, got {other:?}"),
    }
}

#[test]
fn empty_wait_interrupt_result_is_head_line_only() {
    let r = interrupted_wait_tool_result(
        &serde_json::json!({}),
        WaitInterruptCause::HumanInterjection,
    );
    assert_eq!(r.prompt_text, WAIT_INTERRUPTED_HEAD);
    assert!(!r.prompt_text.contains("Interrupted wait set"));
    match &r.output {
        ToolOutput::TaskOutput(TaskOutputOutput::Result(res)) => {
            assert_eq!(res.task_id, "");
            assert_eq!(res.status, "cancelled");
        }
        other => panic!("expected TaskOutput Result, got {other:?}"),
    }
}

#[test]
fn parent_interject_wait_result_is_cancelled_with_the_agent_head() {
    let r = interrupted_wait_tool_result(
        &serde_json::json!({"task_ids": ["bg-9"], "timeout_ms": 60_000}),
        WaitInterruptCause::ParentInterject,
    );
    assert!(
        r.prompt_text
            .starts_with(WAIT_INTERRUPTED_BY_AGENT_MESSAGE_HEAD)
    );
    assert_eq!(
        interrupted_wait_prompt(true, WaitInterruptCause::ParentInterject),
        r.prompt_text
    );
    match &r.output {
        ToolOutput::TaskOutput(TaskOutputOutput::Result(res)) => {
            assert_eq!(
                ("bg-9", "cancelled"),
                (res.task_id.as_str(), res.status.as_str())
            );
        }
        other => panic!("expected TaskOutput Result, got {other:?}"),
    }
}

fn user_message() -> PendingInterjection<agent_client_protocol::ImageContent> {
    PendingInterjection {
        text: "user message".to_owned(),
        attachments: Vec::new(),
    }
}

/// The `biased` select prefers an already-finished wait result over either interrupt.
async fn race_wait(
    human: &InterjectionBuffer<agent_client_protocol::ImageContent>,
    parent: &ParentInterjectSignal,
    wait: impl Future<Output = &'static str>,
) -> Result<&'static str, WaitInterruptCause> {
    tokio::select! {
        biased;
        result = wait => Ok(result),
        cause = wait_for_wait_interrupt(human, parent) => Err(cause),
    }
}

async fn never_finishes() -> &'static str {
    tokio::time::sleep(Duration::from_secs(3600)).await;
    "wait-result"
}

#[tokio::test(start_paused = true)]
async fn human_interjection_aborts_an_in_flight_wait() {
    let human = InterjectionBuffer::new();
    let parent = ParentInterjectSignal::default();
    human.push(user_message());
    assert_eq!(
        Err(WaitInterruptCause::HumanInterjection),
        race_wait(&human, &parent, never_finishes()).await
    );
    assert_eq!(
        Ok("wait-result"),
        race_wait(&human, &parent, async { "wait-result" }).await
    );
}

#[tokio::test(start_paused = true)]
async fn parent_interject_signal_aborts_an_in_flight_wait() {
    let human = InterjectionBuffer::new();
    let parent = ParentInterjectSignal::default();
    parent.set_pending(true);
    assert_eq!(
        Err(WaitInterruptCause::ParentInterject),
        race_wait(&human, &parent, never_finishes()).await
    );
    parent.set_pending(false);
    let unresolved = tokio::time::timeout(
        Duration::from_millis(500),
        race_wait(&human, &parent, never_finishes()),
    )
    .await;
    assert!(
        unresolved.is_err(),
        "a cleared signal must not abort the wait"
    );
}

#[tokio::test(start_paused = true)]
async fn human_cause_wins_when_both_are_pending() {
    let human = InterjectionBuffer::new();
    human.push(user_message());
    let parent = ParentInterjectSignal::default();
    parent.set_pending(true);
    assert_eq!(
        Err(WaitInterruptCause::HumanInterjection),
        race_wait(&human, &parent, never_finishes()).await
    );
}

#[test]
fn wait_task_ids_from_args_matches_wait_tool_lenient_parse() {
    assert_eq!(
        wait_task_ids_from_args(&serde_json::json!({"task_ids": "solo"})),
        ids(&["solo"])
    );
    assert_eq!(
        wait_task_ids_from_args(&serde_json::json!({"task_id": 228})),
        ids(&["228"])
    );
    assert_eq!(
        wait_task_ids_from_args(&serde_json::json!({"task_ids": [" a ", "b"]})),
        ids(&["a", "b"])
    );
}

#[test]
fn wait_ids_after_interrupt_drops_proper_superset_only() {
    let interrupted = ids(&["orig-a", "orig-b", "orig-c"]);
    let requested = ids(&["orig-a", "orig-b", "orig-c", "extra"]);
    assert_eq!(
        wait_ids_after_interrupt(&interrupted, &requested),
        Some(interrupted.clone())
    );
    assert_eq!(
        wait_ids_after_interrupt(&interrupted, &interrupted),
        None,
        "exact resume is already the interrupted set"
    );
    assert_eq!(
        wait_ids_after_interrupt(&interrupted, &ids(&["extra"])),
        None,
        "an explicit wait on only the new work must not be rewritten"
    );
    assert_eq!(
        wait_ids_after_interrupt(&interrupted, &ids(&["orig-a"])),
        None,
        "a subset wait is a new choice, not a resume-plus-extras"
    );
    assert_eq!(wait_ids_after_interrupt(&[], &requested), None);
    assert_eq!(wait_ids_after_interrupt(&interrupted, &[]), None);
    assert_eq!(
        wait_ids_after_interrupt(&interrupted, &ids(&["orig-a", "extra"])),
        None,
        "mixed overlap plus extras is a new choice"
    );
}

#[test]
fn apply_rewrite_drops_singular_task_id() {
    let state = BlockingWaitState::new();
    state.note_interrupted_wait(ids(&["orig-a"]));
    let mut args = serde_json::json!({
        "task_id": "orig-a",
        "task_ids": ["orig-a", "extra"],
        "timeout_ms": 60_000
    });
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut args),
        InterruptedWaitFilter::Rewritten {
            kept: ids(&["orig-a"]),
            requested: 2,
        }
    );
    assert_eq!(
        args.pointer("/task_ids")
            .unwrap_or(&serde_json::Value::Null),
        &serde_json::json!(["orig-a"])
    );
    assert!(args.get("task_id").is_none());
}

#[test]
fn apply_rewrites_proper_superset_and_keeps_remembered_set() {
    let state = BlockingWaitState::new();
    let interrupted = ids(&["orig-a", "orig-b", "orig-c"]);
    state.note_interrupted_wait(interrupted.clone());
    let mut args = serde_json::json!({
        "task_ids": ["orig-a", "orig-b", "orig-c", "extra"],
        "timeout_ms": 600_000
    });
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut args),
        InterruptedWaitFilter::Rewritten {
            kept: interrupted.clone(),
            requested: 4,
        }
    );
    assert_eq!(
        args.pointer("/task_ids")
            .unwrap_or(&serde_json::Value::Null),
        &serde_json::json!(["orig-a", "orig-b", "orig-c"])
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(interrupted.as_slice())
    );
}

#[test]
fn apply_does_not_forget_so_sibling_superset_still_strips() {
    let state = BlockingWaitState::new();
    let remembered = ids(&["orig-a", "orig-b", "orig-c"]);
    state.note_interrupted_wait(remembered.clone());
    let mut exact = serde_json::json!({"task_ids": ["orig-a", "orig-b", "orig-c"]});
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut exact),
        InterruptedWaitFilter::Unchanged
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(remembered.as_slice())
    );
    let mut sibling = serde_json::json!({
        "task_ids": ["orig-a", "orig-b", "orig-c", "extra"]
    });
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut sibling),
        InterruptedWaitFilter::Rewritten {
            kept: remembered,
            requested: 4,
        }
    );
}

#[test]
fn apply_mixed_overlap_does_not_forget() {
    let state = BlockingWaitState::new();
    let remembered = ids(&["orig-a", "orig-b", "orig-c"]);
    state.note_interrupted_wait(remembered.clone());
    let mut args = serde_json::json!({"task_ids": ["orig-a", "extra"]});
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut args),
        InterruptedWaitFilter::Unchanged
    );
    assert_eq!(
        args.pointer("/task_ids")
            .unwrap_or(&serde_json::Value::Null),
        &serde_json::json!(["orig-a", "extra"])
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(remembered.as_slice())
    );
    let mut later = serde_json::json!({
        "task_ids": ["orig-a", "orig-b", "orig-c", "extra"]
    });
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut later),
        InterruptedWaitFilter::Rewritten {
            kept: remembered,
            requested: 4,
        }
    );
}

#[test]
fn apply_disjoint_wait_does_not_forget() {
    let state = BlockingWaitState::new();
    let remembered = ids(&["orig-a", "orig-b"]);
    state.note_interrupted_wait(remembered.clone());
    let mut args = serde_json::json!({"task_ids": ["babysit"]});
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut args),
        InterruptedWaitFilter::Unchanged
    );
    assert_eq!(
        args.pointer("/task_ids")
            .unwrap_or(&serde_json::Value::Null),
        &serde_json::json!(["babysit"])
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(remembered.as_slice())
    );
}

#[test]
fn concurrent_aborts_union_so_side_work_cannot_replace_implement() {
    let state = BlockingWaitState::new();
    record_interruptible_wait_outcome(
        &state,
        state.generation(),
        ids(&["impl-a", "impl-b"]),
        true,
        &[],
    );
    record_interruptible_wait_outcome(&state, state.generation(), ids(&["babysit"]), true, &[]);
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(ids(&["impl-a", "impl-b", "babysit"]).as_slice())
    );
    let mut args = serde_json::json!({
        "task_ids": ["impl-a", "impl-b", "babysit", "extra"]
    });
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut args),
        InterruptedWaitFilter::Rewritten {
            kept: ids(&["impl-a", "impl-b", "babysit"]),
            requested: 4,
        }
    );
}

#[test]
fn complete_disjoint_wait_does_not_forget() {
    let state = BlockingWaitState::new();
    let remembered = ids(&["orig-a", "orig-b"]);
    state.note_interrupted_wait(remembered.clone());
    record_interruptible_wait_outcome(
        &state,
        state.generation(),
        ids(&["other"]),
        false,
        &ids(&["other"]),
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(remembered.as_slice())
    );
}

#[test]
fn abort_notes_filtered_ids_so_second_superset_still_strips() {
    let state = BlockingWaitState::new();
    state.note_interrupted_wait(ids(&["orig-a", "orig-b"]));
    let mut args = serde_json::json!({"task_ids": ["orig-a", "orig-b", "extra"]});
    apply_interrupted_wait_filter(&state, &mut args);
    record_interruptible_wait_outcome(
        &state,
        state.generation(),
        wait_task_ids_from_args(&args),
        true,
        &[],
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(ids(&["orig-a", "orig-b"]).as_slice())
    );
    let mut second = serde_json::json!({"task_ids": ["orig-a", "orig-b", "extra"]});
    assert!(matches!(
        apply_interrupted_wait_filter(&state, &mut second),
        InterruptedWaitFilter::Rewritten { .. }
    ));
    assert_eq!(
        second
            .pointer("/task_ids")
            .unwrap_or(&serde_json::Value::Null),
        &serde_json::json!(["orig-a", "orig-b"])
    );
}

#[test]
fn complete_drops_only_finished_ids() {
    let state = BlockingWaitState::new();
    record_interruptible_wait_outcome(
        &state,
        state.generation(),
        ids(&["orig-a", "orig-b"]),
        true,
        &[],
    );
    record_interruptible_wait_outcome(&state, state.generation(), ids(&["babysit"]), true, &[]);
    record_interruptible_wait_outcome(
        &state,
        state.generation(),
        ids(&["orig-a", "orig-b"]),
        false,
        &ids(&["orig-a", "orig-b"]),
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(ids(&["babysit"]).as_slice())
    );
}

#[test]
fn not_found_drops_dead_ids() {
    let state = BlockingWaitState::new();
    state.note_interrupted_wait(ids(&["gone", "orig-b"]));
    record_interruptible_wait_outcome(
        &state,
        state.generation(),
        ids(&["gone", "orig-b"]),
        false,
        &ids(&["gone"]),
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(ids(&["orig-b"]).as_slice())
    );
}

#[test]
fn timeout_does_not_forget_running_ids() {
    let state = BlockingWaitState::new();
    let remembered = ids(&["orig-a", "orig-b"]);
    state.note_interrupted_wait(remembered.clone());
    record_interruptible_wait_outcome(&state, state.generation(), remembered.clone(), false, &[]);
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(remembered.as_slice())
    );
}

#[test]
fn reset_invalidates_late_abort_record() {
    let state = BlockingWaitState::new();
    let stale = state.generation();
    state.reset();
    record_interruptible_wait_outcome(&state, stale, ids(&["orig-a"]), true, &[]);
    assert_eq!(state.interrupted_wait_ids(), None);
}

#[test]
fn complete_of_full_set_forgets() {
    let state = BlockingWaitState::new();
    state.note_interrupted_wait(ids(&["orig-a", "orig-b"]));
    record_interruptible_wait_outcome(
        &state,
        state.generation(),
        ids(&["orig-a", "orig-b"]),
        false,
        &ids(&["orig-a", "orig-b"]),
    );
    let mut args = serde_json::json!({"task_ids": ["orig-a", "orig-b", "extra"]});
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut args),
        InterruptedWaitFilter::Unchanged
    );
}

#[test]
fn empty_requested_ids_do_not_consume_the_set() {
    let state = BlockingWaitState::new();
    state.note_interrupted_wait(ids(&["orig-a"]));
    let mut args = serde_json::json!({"shell_id": "sh-1", "block_until_ms": 0});
    assert_eq!(
        apply_interrupted_wait_filter(&state, &mut args),
        InterruptedWaitFilter::Unchanged
    );
    assert_eq!(
        args.pointer("/shell_id")
            .unwrap_or(&serde_json::Value::Null),
        "sh-1"
    );
    assert_eq!(
        state.interrupted_wait_ids().as_deref(),
        Some(ids(&["orig-a"]).as_slice())
    );
}
