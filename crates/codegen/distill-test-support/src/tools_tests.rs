// Modified for Distill by Samuel Fajreldines, 2026.
use super::{CRON_DEFAULT_INTERVAL, FIRST_TASK_ID, PickedToolCall, Tool};
use crate::inference_request::{HistoryToolCall, OfferedTools, offered_tools};
use serde_json::{Value, json};
fn offered(names: &[&str]) -> OfferedTools {
    let tools: Vec<serde_json::Value> = names
        .iter()
        .map(|name| json!({ "type": "function", "name": name }))
        .collect();
    offered_tools(&json!({ "tools": tools }))
}
#[test]
fn pick_uses_distill_name_else_none_when_unoffered() {
    let cases: [(Tool, &[&str], Option<&str>); 4] = [
        (Tool::Read, &["Read", "read_file"], Some("read_file")),
        (Tool::Read, &["read_file"], Some("read_file")),
        (Tool::Shell, &["read_file", "Read"], None),
        (Tool::Skill, &["Read"], None),
    ];
    for (tool, names, expected) in cases {
        let picked = tool.pick(&offered(names), &json!({}));
        assert_eq!(
            expected.map(str::to_owned),
            picked.map(|call| call.name),
            "{tool:?} offered {names:?}"
        );
    }
}
#[test]
fn every_kind_with_a_distill_name_resolves_under_it() {
    let cases: [(Tool, &str); 27] = [
        (Tool::Shell, "run_terminal_command"),
        (Tool::Read, "read_file"),
        (Tool::Edit, "search_replace"),
        (Tool::Write, "write"),
        (Tool::Grep, "grep"),
        (Tool::Glob, "glob"),
        (Tool::List, "list_dir"),
        (Tool::MemorySearch, "memory_search"),
        (Tool::MemoryGet, "memory_get"),
        (Tool::Task, "spawn_subagent"),
        (Tool::Skill, "skill"),
        (Tool::SendMessage, "send_subagent_message"),
        (Tool::EnterPlanMode, "enter_plan_mode"),
        (Tool::ExitPlanMode, "exit_plan_mode"),
        (Tool::WebFetch, "web_fetch"),
        (Tool::WebSearch, "web_search"),
        (Tool::Question, "ask_user_question"),
        (Tool::Todo, "todo_write"),
        (Tool::SearchTool, "search_tool"),
        (Tool::KillTask, "kill_command_or_subagent"),
        (Tool::SchedulerCreate, "scheduler_create"),
        (Tool::SchedulerList, "scheduler_list"),
        (Tool::SchedulerDelete, "scheduler_delete"),
        (Tool::Workflow, "workflow"),
        (Tool::TaskCreate, "todo_write"),
        (Tool::TaskUpdate, "todo_write"),
        (Tool::CronCreate, "scheduler_create"),
    ];
    for (tool, name) in cases {
        let picked = tool.pick(&offered(&[name]), &json!({}));
        assert_eq!(
            Some(name.to_owned()),
            picked.map(|call| call.name),
            "{tool:?}"
        );
    }
}
#[test]
fn required_fields_the_case_omits_are_filled_on_distill() {
    let cases: [(Tool, &str, Value, Value); 7] = [
        (
            Tool::Shell,
            "run_terminal_command",
            json!({ "command": "touch x" }),
            json!({ "command": "touch x", "description": "touch x" }),
        ),
        (
            Tool::Task,
            "spawn_subagent",
            json!({ "prompt": "What color?", "subagent_type": "oracle" }),
            json!({ "prompt": "What color?", "subagent_type": "oracle", "description": "What color?" }),
        ),
        (
            Tool::Question,
            "ask_user_question",
            json!({ "questions": [{ "question": "Tea or coffee?", "options": [{ "label": "Tea" }] }] }),
            json!({ "questions": [{ "question": "Tea or coffee?", "options": [{ "label": "Tea", "description": "Tea" }] }] }),
        ),
        (
            Tool::Todo,
            "todo_write",
            json!({ "todos": [{ "id": "t1", "content": "First" }] }),
            json!({ "todos": [{ "id": "t1", "content": "First", "status": "pending" }] }),
        ),
        (
            Tool::TaskCreate,
            "todo_write",
            json!({ "subject": "Implement login", "description": "Add the endpoint" }),
            json!({ "todos": [{ "id": FIRST_TASK_ID, "content": "Implement login", "status": "pending" }] }),
        ),
        (
            Tool::TaskUpdate,
            "todo_write",
            json!({ "status": "completed" }),
            json!({ "todos": [{ "id": FIRST_TASK_ID, "status": "completed" }] }),
        ),
        (
            Tool::CronCreate,
            "scheduler_create",
            json!({ "schedule": "0 9 * * 1-5", "prompt": "check the build" }),
            json!({ "schedule": "0 9 * * 1-5", "prompt": "check the build", "interval": CRON_DEFAULT_INTERVAL }),
        ),
    ];
    for (tool, distill_name, arguments, expected) in cases {
        let picked = tool.pick(&offered(&[distill_name]), &arguments).unwrap();
        assert_eq!(expected, picked.arguments, "{tool:?}");
    }
}
#[test]
fn field_the_case_wrote_is_kept_over_its_fill() {
    let cases: [(Tool, &str, Value); 3] = [
        (
            Tool::Shell,
            "run_terminal_command",
            json!({ "command": "touch x", "description": "make x" }),
        ),
        (
            Tool::Todo,
            "todo_write",
            json!({ "todos": [{ "id": "t1", "content": "First", "status": "completed" }] }),
        ),
        (
            Tool::CronCreate,
            "scheduler_create",
            json!({ "interval": "5m", "prompt": "check" }),
        ),
    ];
    for (tool, distill_name, arguments) in cases {
        let picked = tool.pick(&offered(&[distill_name]), &arguments).unwrap();
        assert_eq!(arguments, picked.arguments, "{tool:?}");
    }
}
#[test]
fn task_call_keeps_the_id_it_names() {
    let created = Tool::TaskCreate
        .pick(
            &offered(&["todo_write"]),
            &json!({ "id": "7", "subject": "Ship it" }),
        )
        .unwrap();
    let updated = Tool::TaskUpdate
        .pick(
            &offered(&["todo_write"]),
            &json!({ "taskId": "7", "status": "in_progress", "subject": "Ship it now" }),
        )
        .unwrap();
    assert_eq!(
        (
            json!({ "todos": [{ "id": "7", "content": "Ship it", "status": "pending" }] }),
            json!({ "todos": [{ "id": "7", "content": "Ship it now", "status": "in_progress" }] })
        ),
        (created.arguments, updated.arguments)
    );
}
#[test]
fn arguments_that_are_not_a_table_pass_through_on_distill() {
    let arguments = json!("raw");
    let picked = Tool::Shell
        .pick(&offered(&["run_terminal_command"]), &arguments)
        .unwrap();
    assert_eq!(arguments, picked.arguments);
}
#[test]
fn mcp_pick_routes_through_use_tool_when_only_the_meta_tool_is_offered() {
    let cases: [(&[&str], Option<PickedToolCall>); 3] = [
        (
            &["linear__save_issue", "use_tool"],
            Some(PickedToolCall {
                name: "linear__save_issue".to_owned(),
                arguments: json!({ "title": "x" }),
            }),
        ),
        (
            &["use_tool", "search_tool"],
            Some(PickedToolCall {
                name: "use_tool".to_owned(),
                arguments: json!({
                    "tool_name": "linear__save_issue",
                    "tool_input": { "title": "x" },
                }),
            }),
        ),
        (&["read_file"], None),
    ];
    for (names, expected) in cases {
        let picked =
            Tool::mcp("linear", "save_issue").pick(&offered(names), &json!({ "title": "x" }));
        assert_eq!(expected, picked, "offered {names:?}");
    }
}
#[test]
fn history_use_tool_call_is_the_mcp_tool_named_in_its_tool_name() {
    let cases: [(Tool, &str, Value, bool); 3] = [
        (
            Tool::mcp("linear", "save_issue"),
            "use_tool",
            json!({ "tool_name": "linear__save_issue", "tool_input": {} }),
            true,
        ),
        (
            Tool::mcp("linear", "list_issues"),
            "use_tool",
            json!({ "tool_name": "linear__save_issue", "tool_input": {} }),
            false,
        ),
        (
            Tool::mcp("linear", "save_issue"),
            "linear__save_issue",
            json!({}),
            true,
        ),
    ];
    for (tool, name, arguments, expected) in cases {
        let call = HistoryToolCall {
            name: name.to_owned(),
            arguments,
        };
        assert_eq!(
            expected,
            tool.is_called_by(&call),
            "{tool:?} called as {name}"
        );
    }
}
