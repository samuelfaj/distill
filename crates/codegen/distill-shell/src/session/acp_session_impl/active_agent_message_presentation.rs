// Modified for Distill by Samuel Fajreldines, 2026.
//! Tool-call display for active-agent messages; it never includes the message content.

use agent_client_protocol as acp;
use distill_tools::implementations::distill::send_subagent_message::SendSubagentMessageInput;

pub(super) fn active_agent_message_tool_call_display(
    _message: &SendSubagentMessageInput,
) -> (String, acp::ToolKind) {
    (
        "Sending message to subagent".to_owned(),
        acp::ToolKind::Other,
    )
}

#[cfg(test)]
#[path = "active_agent_message_presentation_tests.rs"]
mod tests;
