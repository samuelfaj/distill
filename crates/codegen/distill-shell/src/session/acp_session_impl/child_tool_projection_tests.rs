// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use distill_tools::implementations::distill::SEND_SUBAGENT_MESSAGE_TOOL_NAME;

fn tool(name: &str, description: Option<&str>, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.map(str::to_owned),
        parameters,
    }
}

fn specs() -> Vec<ToolSpec> {
    vec![
        tool(
            "read_file",
            Some("read"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            "relay_to_subagent",
            Some("renamed message"),
            serde_json::json!({"required": ["text"]}),
        ),
        tool(
            "grep",
            None,
            serde_json::json!({"type": "object", "required": ["pattern"]}),
        ),
    ]
}

/// The child bridge as the projection sees it: anything it does not register (parent-only tools) is unknown.
fn kind_for_name(name: &str) -> Option<ToolKind> {
    match name {
        "read_file" => Some(ToolKind::Read),
        "grep" => Some(ToolKind::Search),
        "relay_to_subagent" => Some(ToolKind::ActiveAgentMessage),
        _ => None,
    }
}

#[test]
fn rebuilt_projection_removes_renamed_active_message_tool() {
    let projected = child_safe_tool_specs(
        specs(),
        ChildToolProjection::Rebuilt,
        ChildMessagingGrant::Ungranted,
        kind_for_name,
    );

    let [read, grep] = projected.as_slice() else {
        panic!("expected two projected tools: {projected:?}");
    };
    assert_eq!(read.name, "read_file");
    assert_eq!(read.description.as_deref(), Some("read"));
    assert_eq!(grep.name, "grep");
    assert_eq!(
        grep.parameters,
        serde_json::json!({"type": "object", "required": ["pattern"]})
    );
}

#[test]
fn granted_projection_keeps_only_child_resolvable_tools() {
    let parent = specs();
    let projected = child_safe_tool_specs(
        parent,
        ChildToolProjection::VerbatimMirror,
        ChildMessagingGrant::Granted,
        kind_for_name,
    );
    assert_eq!(
        projected
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read_file", "relay_to_subagent", "grep"]
    );
}

#[test]
fn verbatim_mirror_projection_strips_root_only_keeps_ordinary_byte_identical() {
    // Ordinary tools pass through unchanged, so the child's specs serialize to the parent's exact bytes and the radix cache stays aligned
    // The active-message tool exists only at the root, so even the mirror drops it: by kind when renamed, as unresolvable when the child never registered it
    let parent = vec![
        tool(
            "read_file",
            Some("read"),
            serde_json::json!({"type": "object"}),
        ),
        tool(
            "relay_to_subagent",
            Some("renamed message"),
            serde_json::json!({"required": ["text"]}),
        ),
        tool(
            SEND_SUBAGENT_MESSAGE_TOOL_NAME,
            Some("canonical message"),
            serde_json::json!({"required": ["subagent_id", "message"]}),
        ),
        tool(
            "grep",
            None,
            serde_json::json!({"type": "object", "required": ["pattern"]}),
        ),
    ];

    let projected = child_safe_tool_specs(
        parent.clone(),
        ChildToolProjection::VerbatimMirror,
        ChildMessagingGrant::Ungranted,
        kind_for_name,
    );
    let [first, _, _, last] = parent.as_slice() else {
        panic!("expected four parent tools: {parent:?}");
    };
    let expected = vec![first.clone(), last.clone()];

    assert_eq!(
        projected
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read_file", "grep"]
    );
    assert_eq!(
        serde_json::to_vec(&projected).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
}

#[test]
fn verbatim_mirror_drops_parent_only_tools_the_child_cannot_resolve() {
    // Plan mode and ask-user exist only in the parent's bridge; the mirror must not advertise tools the child cannot run
    let parent = vec![
        tool("read_file", Some("read"), serde_json::json!({})),
        tool("enter_plan_mode", Some("plan"), serde_json::json!({})),
        tool("exit_plan_mode", Some("plan"), serde_json::json!({})),
        tool("ask_user_question", Some("ask"), serde_json::json!({})),
        tool("grep", None, serde_json::json!({})),
    ];

    let projected = child_safe_tool_specs(
        parent.clone(),
        ChildToolProjection::VerbatimMirror,
        ChildMessagingGrant::Ungranted,
        kind_for_name,
    );

    assert_eq!(
        projected
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read_file", "grep"]
    );
    assert_eq!(
        serde_json::to_vec(&projected).unwrap(),
        serde_json::to_vec(&[
            parent.first().expect("parent tool 0").clone(),
            parent.get(4).expect("parent tool 4").clone(),
        ])
        .unwrap()
    );
}
