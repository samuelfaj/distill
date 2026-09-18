//! Wiring tests for MCP tool-layer images through `handle_bridge_tool_success`.
use super::support::*;
use super::*;
use xai_grok_sampling_types::{ContentPart, ConversationItem};
use xai_grok_tools::types::output::{MCPOutput, ToolOutput, ToolRunResult};
use xai_grok_tools::util::base64_images::{ExtractedImage, IMAGE_CONTENT_PLACEHOLDER};
/// A 32×32 solid PNG, above the vision minimum side and area, so normalize keeps it.
fn vision_ok_png_b64() -> String {
    use base64::Engine as _;
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(32, 32, Rgba([128, 64, 32, 255]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("encode png");
    base64::engine::general_purpose::STANDARD.encode(buf)
}
fn mcp_screenshot_result(payload_b64: &str) -> ToolRunResult {
    let mut mcp = MCPOutput::okay_output(
        "browser_screenshot".into(),
        "browser-use".into(),
        IMAGE_CONTENT_PLACEHOLDER.into(),
    );
    mcp.extracted_images = vec![ExtractedImage {
        data: payload_b64.to_owned(),
        mime_type: "image/png".into(),
    }];
    ToolRunResult {
        output: ToolOutput::MCP(mcp),
        prompt_text: IMAGE_CONTENT_PLACEHOLDER.into(),
        effective_tool_name: None,
    }
}
fn tool_result_text(item: &ConversationItem) -> &str {
    match item {
        ConversationItem::ToolResult(tr) => tr.content.as_ref(),
        other => panic!("expected ToolResult, got {other:?}"),
    }
}
fn last_tool_result_text(conv: &[ConversationItem]) -> &str {
    let tool = conv
        .iter()
        .rev()
        .find(|item| matches!(item, ConversationItem::ToolResult(_)))
        .expect("tool result pushed");
    tool_result_text(tool)
}
fn followup_has_data_image(followups: &[ConversationItem]) -> bool {
    followups.iter().any(|item| match item {
        ConversationItem::User(u) => u
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Image { url } if url.starts_with("data:image/"))),
        _ => false,
    })
}
/// Multimodal: the image drained from the MCP output becomes a deferred vision follow-up; the tool result text keeps the placeholder.
#[tokio::test(flavor = "current_thread")]
async fn handle_bridge_tool_success_multimodal_mcp_image_deferred_followup() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                xai_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx)
                .await;
            assert!(!actor.is_cursor_harness());
            let payload = vision_ok_png_b64();
            let parsed_args = serde_json::json!({});
            let followups = actor
                .handle_bridge_tool_success(BridgeToolSuccess {
                    tool_call_id: &acp::ToolCallId::new("tc-mcp-img"),
                    call_id: "tc-mcp-img",
                    requested_tool_name: "browser_screenshot",
                    effective_tool_name: "browser_screenshot",
                    drained: DrainedToolSuccess::new(mcp_screenshot_result(&payload)),
                    concatenated_json_count: 0,
                    model_id: "test-model",
                    tool_parsed_args: &parsed_args,
                    model_output_override: None,
                })
                .await
                .expect("bridge success");
            assert!(
                followup_has_data_image(&followups),
                "multimodal must attach drained MCP image as deferred vision follow-up: {followups:?}"
            );
            assert!(
                followups.iter().any(|item| matches!(
                    item,
                    ConversationItem::User(u) if u
                        .content
                        .iter()
                        .any(|p| matches!(p, ContentPart::Text { text } if text.contains("Image extracted from tool result")))
                )),
                "expected extracted-image caption: {followups:?}"
            );
            let conv = actor.chat_state_handle.get_conversation().await;
            let tool = conv
                .iter()
                .rev()
                .find(|i| matches!(i, ConversationItem::ToolResult(_)))
                .expect("tool result pushed");
            let text = tool_result_text(tool);
            assert!(
                text.contains(IMAGE_CONTENT_PLACEHOLDER),
                "placeholder stays in tool text: {text}"
            );
            assert!(
                !text.contains("data:image"),
                "tool text must not reinject data URI: {text}"
            );
            assert!(
                !text.contains("image omitted"),
                "no budget-omit copy: {text}"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn handle_bridge_tool_success_replacement_drops_images_and_keeps_reminders() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                xai_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx)
                .await;
            let payload = vision_ok_png_b64();
            let mut result = mcp_screenshot_result(&payload);
            result.prompt_text = format!(
                "{IMAGE_CONTENT_PLACEHOLDER}\n\n<system-reminder>\nturn reminder\n</system-reminder>"
            );
            let parsed_args = serde_json::json!({});
            let followups = actor
                .handle_bridge_tool_success(BridgeToolSuccess {
                    tool_call_id: &acp::ToolCallId::new("tc-mcp-replaced"),
                    call_id: "tc-mcp-replaced",
                    requested_tool_name: "browser_screenshot",
                    effective_tool_name: "browser_screenshot",
                    drained: DrainedToolSuccess::new(result),
                    concatenated_json_count: 0,
                    model_id: "test-model",
                    tool_parsed_args: &parsed_args,
                    model_output_override: Some("[redacted]".to_string()),
                })
                .await
                .expect("bridge success");
            assert!(
                !followup_has_data_image(&followups),
                "a replaced output must not hand the model the original's images: {followups:?}"
            );
            let conv = actor.chat_state_handle.get_conversation().await;
            let text = last_tool_result_text(&conv);
            assert!(
                text.starts_with("[redacted]"),
                "the replacement is what the model reads: {text}"
            );
            assert!(
                text.contains("turn reminder"),
                "the turn's reminders survive a replacement: {text}"
            );
            assert!(
                !text.contains(IMAGE_CONTENT_PLACEHOLDER),
                "the replaced rendering is gone: {text}"
            );
        })
        .await;
}
fn prepared_post_tool_use_call(id: &str, tool_name: &str) -> PreparedToolCall {
    PreparedToolCall {
        call_id: id.to_string(),
        tool_call_id: acp::ToolCallId::new(id),
        tool_name: tool_name.to_string(),
        raw_arguments: "{}".to_string(),
        parsed_args: serde_json::json!({}),
        model_id: "test-model".to_string(),
        concatenated_json_count: 0,
        dispatch_target_name: None,
        is_read_only: false,
        rewriting_hook: None,
        additional_context: Vec::new(),
    }
}
fn mcp_text_result(marker: &str) -> ToolRunResult {
    ToolRunResult {
        output: ToolOutput::MCP(MCPOutput::okay_output(
            "search".into(),
            "memory".into(),
            marker.into(),
        )),
        prompt_text: marker.into(),
        effective_tool_name: None,
    }
}
fn bash_text_result(marker: &str) -> ToolRunResult {
    let output: ToolOutput = serde_json::from_value(serde_json::json!({
        "type": "Bash",
        "output": [],
        "output_for_prompt": marker,
        "exit_code": 0,
        "command": "ls",
        "truncated": false,
        "timed_out": false,
        "current_dir": "/tmp",
        "output_file": "",
        "total_bytes": 0,
    }))
    .expect("bash output should deserialize");
    ToolRunResult {
        output,
        prompt_text: marker.into(),
        effective_tool_name: None,
    }
}
#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_replacement_reaches_model_original_stays_on_record() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel::<
                xai_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let (mut actor, mut event_rx) = create_test_actor_ex(
                    0,
                    256_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                )
                .await;
            install_pre_tool_use_hooks(
                &mut actor,
                vec![post_tool_use_spec(
                    "redact",
                    None,
                    r#"echo '{"hookSpecificOutput":{"updatedMCPToolOutput":"[redacted by hook]"}}'"#,
                )],
            );
            let original_marker = "ORIGINAL-mcp-content-xyz";
            let drained = DrainedToolSuccess::new(mcp_text_result(original_marker));
            let prepared = prepared_post_tool_use_call("tc-redact", "search__memory");
            let (mut delivery, _deferred_scrollback) = actor
                .dispatch_post_tool_use_hook(&prepared, drained.output(), None)
                .await;
            let model_output_override = delivery.model_output.take();
            assert_eq!(
                model_output_override.as_deref(),
                Some("[redacted by hook]"),
                "the real dispatch+plan wiring must produce the replacement"
            );
            actor
                .handle_bridge_tool_success(BridgeToolSuccess {
                    tool_call_id: &acp::ToolCallId::new("tc-redact"),
                    call_id: "tc-redact",
                    requested_tool_name: "search__memory",
                    effective_tool_name: "search__memory",
                    drained,
                    concatenated_json_count: 0,
                    model_id: "test-model",
                    tool_parsed_args: &serde_json::json!({}),
                    model_output_override,
                })
                .await
                .expect("bridge success");
            let conv = actor.chat_state_handle.get_conversation().await;
            let text = last_tool_result_text(&conv);
            assert!(
                text.contains("[redacted by hook]"),
                "the model reads the replacement: {text}"
            );
            assert!(
                !text.contains(original_marker),
                "the model must not see the original: {text}"
            );
            let mut acp_dump = String::new();
            while let Ok(event) = event_rx.try_recv() {
                if let SessionEvent::Notification(SessionNotification::Acp(n)) = event
                    && let Ok(v) = serde_json::to_value(&n.update)
                {
                    acp_dump.push_str(&v.to_string());
                }
            }
            assert!(
                acp_dump.contains(original_marker),
                "the ACP session/update must retain the original output: {acp_dump}"
            );
            assert!(
                !acp_dump.contains("[redacted by hook]"),
                "the replacement must not reach the ACP record: {acp_dump}"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_rejection_downgrades_only_the_producing_run() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel::<
                xai_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let (_acp_updates, xai_updates) = spawn_capturing_gateway_loop(gateway_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx)
                .await;
            install_pre_tool_use_hooks(
                &mut actor,
                vec![
                    post_tool_use_spec(
                        "redact",
                        None,
                        r#"echo '{"hookSpecificOutput":{"updatedToolOutput":{"not":"a tool output"}}}'"#,
                    ),
                    post_tool_use_spec(
                        "redact",
                        None,
                        r#"echo '{"hookSpecificOutput":{"additionalContext":"noted"}}'"#,
                    ),
                ],
            );
            let drained = DrainedToolSuccess::new(bash_text_result("original"));
            let prepared = prepared_post_tool_use_call(
                "tc-reject",
                "run_terminal_command",
            );
            let (_delivery, deferred_scrollback) = actor
                .dispatch_post_tool_use_hook(&prepared, drained.output(), None)
                .await;
            if let Some(scrollback) = deferred_scrollback {
                actor.emit_post_tool_use_scrollback(scrollback).await;
            }
            drain_gateway_turns().await;
            let updates = xai_updates.lock().unwrap();
            let mut failed = 0usize;
            let mut success = 0usize;
            for update in updates.iter() {
                let Some(runs) = update.get("runs").and_then(|r| r.as_array()) else {
                    continue;
                };
                for run in runs {
                    if run.get("name").and_then(serde_json::Value::as_str)
                        != Some("redact")
                    {
                        continue;
                    }
                    match run
                        .get("status")
                        .and_then(|s| s.get("status"))
                        .and_then(serde_json::Value::as_str)
                    {
                        Some("failed") => failed += 1,
                        Some("success") => success += 1,
                        other => panic!("unexpected run status {other:?} in {update:?}"),
                    }
                }
            }
            assert_eq!(
                (failed, success),
                (1, 1),
                "only the producing run is downgraded; the sibling same-named run stays success \
                 (failed={failed}, success={success}): {:?}",
                *updates
            );
        })
        .await;
}
