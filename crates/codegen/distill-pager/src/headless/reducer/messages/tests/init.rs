//! Tests for the init system message, available-commands updates, and the skills list derived from them.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn messages_init_is_deferred_and_carries_tools() {
    let mut r = messages(false);
    assert!(
        r.reduce(StreamEvent::AvailableCommands {
            tools: vec!["read_file".into(), "bash".into()],
            commands: vec!["review".into()],
            skills: Vec::new(),
        })
        .is_empty()
    );
    let out = r.reduce(StreamEvent::AgentMessage("hi".into()));
    let Some(init) = out.first() else {
        panic!("expected init message: {out:?}");
    };
    assert_eq!(msg_type(init), Some("system"));
    assert_eq!(json_str(init, "/subtype"), Some("init"));
    assert_eq!(json_str(init, "/model"), Some("grok-4"));
    assert_eq!(json_str(init, "/permissionMode"), Some("bypassPermissions"));
    assert_eq!(json_str(init, "/tools/0"), Some("read_file"));
    assert_eq!(json_str(init, "/slash_commands/0"), Some("review"));
    assert_eq!(json_str(init, "/mcp_servers/0/name"), Some("linear"));
    assert_eq!(json_str(init, "/mcp_servers/0/status"), Some("connected"));
    assert_eq!(json_str(init, "/apiKeySource"), Some("user"));
    assert!(init.get("skills").is_some_and(Value::is_array));
    assert!(init.get("claude_code_version").is_none_or(Value::is_null));
    assert!(init.get("output_style").is_none_or(Value::is_null));
    assert!(init.get("plugins").is_none_or(Value::is_null));
    assert!(
        !r.reduce(StreamEvent::AgentMessage(" there".into()))
            .iter()
            .any(|m| msg_type(m) == Some("system"))
    );
}

#[test]
fn skill_names_extracts_only_skill_commands() {
    let commands = vec![
        builtin_command("clear"),
        skill_command("pdf"),
        workflow_command("ship-it"),
        skill_command("brainstorm"),
    ];
    assert_eq!(skill_names(&commands), vec!["pdf", "brainstorm"]);
}

#[test]
fn messages_init_carries_real_skills() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AvailableCommands {
        tools: vec!["bash".into()],
        commands: vec!["clear".into(), "pdf".into(), "brainstorm".into()],
        skills: vec!["pdf".into(), "brainstorm".into()],
    });
    let out = r.reduce(StreamEvent::AgentMessage("hi".into()));
    let Some(init) = out.first() else {
        panic!("expected init message: {out:?}");
    };
    assert_eq!(json_str(init, "/subtype"), Some("init"));
    assert_eq!(json_str(init, "/skills/0"), Some("pdf"));
    assert_eq!(json_str(init, "/skills/1"), Some("brainstorm"));
}

#[test]
fn messages_init_skills_fallback_is_empty() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AvailableCommands {
        tools: vec!["bash".into()],
        commands: vec!["clear".into()],
        skills: Vec::new(),
    });
    let out = r.reduce(StreamEvent::AgentMessage("hi".into()));
    let Some(init) = out.first() else {
        panic!("expected init message: {out:?}");
    };
    assert_eq!(json_str(init, "/subtype"), Some("init"));
    assert_eq!(init.get("skills"), Some(&json!([])));
}

#[test]
fn messages_init_maps_permission_mode_and_api_key_source() {
    let mut r = MessagesReducer::new();
    r.begin(SessionContext {
        session_id: "s".into(),
        model: None,
        cwd: "/c".into(),
        permission_mode: Some("auto".into()),
        mcp_servers: Vec::new(),
        include_partial_messages: false,
        api_key_auth: false,
        context_window: None,
    });
    let out = r.reduce(StreamEvent::AgentMessage("hi".into()));
    let Some(init) = out.first() else {
        panic!("expected init message: {out:?}");
    };
    assert_eq!(json_str(init, "/permissionMode"), Some("default"));
    assert_eq!(json_str(init, "/apiKeySource"), Some("oauth"));
    assert!(
        init.get("model").is_some_and(Value::is_string),
        "{:?}",
        init.get("model")
    );
}

#[test]
fn messages_skills_stay_subset_when_later_command_update_is_empty() {
    let mut r = messages(false);
    r.reduce(StreamEvent::AvailableCommands {
        tools: vec!["bash".into()],
        commands: vec!["review".into(), "pdf".into()],
        skills: vec!["pdf".into()],
    });
    r.reduce(StreamEvent::AvailableCommands {
        tools: Vec::new(),
        commands: Vec::new(),
        skills: Vec::new(),
    });
    let out = r.reduce(StreamEvent::AgentMessage("hi".into()));
    let Some(init) = out.first() else {
        panic!("expected init message: {out:?}");
    };
    let cmds: Vec<String> = init
        .get("slash_commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let skills: Vec<String> = init
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    assert!(skills.contains(&"pdf".to_string()));
    for s in &skills {
        assert!(
            cmds.contains(s),
            "skill {s} escaped slash_commands {cmds:?}"
        );
    }
}
