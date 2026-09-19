use super::*;
use crate::session::plan_mode::PromptMode;
#[test]
fn human_unsupported_operation_rejects_without_command() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let handle = MessageDeliveryHandle::new(cmd_tx, "resident".to_owned());
    let (respond_to, _) = oneshot::channel();
    let content = HumanPromptContent {
        prompt_blocks: vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new("hello"),
        )],
        prompt_mode: PromptMode::Agent,
        artifact_upload_ctx: None,
        client_identifier: None,
        screen_mode: None,
        verbatim: false,
        traceparent: None,
        json_schema: None,
        tool_overrides_update: None,
        respond_to,
        parsed_prompt_tx: None,
    };
    let envelope = DeliveryEnvelope::from_human(
        Operation::Interject,
        content,
        human_delivery_identity("prompt".to_owned()),
        ResidentHumanGrant::new("resident".to_owned()),
    );

    assert!(matches!(
        handle.send_human(envelope),
        Err(HumanDeliveryError::Unsupported)
    ));
    assert!(cmd_rx.try_recv().is_err());
}
