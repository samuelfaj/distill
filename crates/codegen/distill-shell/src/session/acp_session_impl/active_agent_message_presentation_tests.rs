use super::*;

#[test]
fn pending_title_is_content_free() {
    let text = "private follow-up";
    let input = SendSubagentMessageInput {
        subagent_id: "sub-1".into(),
        text: text.into(),
        delivery: None,
        queue: false,
    };

    let (title, kind) = active_agent_message_tool_call_display(&input);

    assert_eq!(title, "Sending message to subagent");
    assert_eq!(kind, acp::ToolKind::Other);
    assert!(!title.contains(text));
}
