// Modified for Distill by Samuel Fajreldines, 2026.
//! Classify one stream item for `StreamSpanTiming::hold_until_first_content`.

use distill_sampling_types::{ChatCompletionChunk, messages, rs};

use crate::span_timing::ItemClass;

fn chat_chunk_has_content(chunk: &ChatCompletionChunk) -> bool {
    use distill_sampling_types::ChatChunkDelta;
    chunk.choices.iter().any(|choice| {
        let ChatChunkDelta {
            role: _,
            content,
            reasoning_content,
            tool_calls,
            tool_call_id: _,
        } = &choice.delta;
        content.as_deref().is_some_and(|text| !text.is_empty())
            || reasoning_content
                .as_deref()
                .is_some_and(|text| !text.is_empty())
            || tool_calls.iter().any(|call| {
                call.function.as_ref().is_some_and(|function| {
                    function
                        .name
                        .as_deref()
                        .is_some_and(|name| !name.is_empty())
                        || function
                            .arguments
                            .as_deref()
                            .is_some_and(|args| !args.is_empty())
                })
            })
    })
}

fn responses_event_has_content(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent as Event;
    match event {
        Event::ResponseReasoningTextDelta(event) => !event.delta.is_empty(),
        Event::ResponseReasoningSummaryTextDelta(event) => !event.delta.is_empty(),
        Event::ResponseOutputTextDelta(event) => !event.delta.is_empty(),
        Event::ResponseRefusalDelta(event) => !event.delta.is_empty(),
        Event::ResponseFunctionCallArgumentsDelta(event) => !event.delta.is_empty(),
        Event::ResponseCustomToolCallInputDelta(event) => !event.delta.is_empty(),
        Event::ResponseMCPCallArgumentsDelta(event) => !event.delta.is_empty(),
        Event::ResponseCodeInterpreterCallCodeDelta(event) => !event.delta.is_empty(),
        Event::ResponseOutputItemAdded(event) => output_item_is_tool_call(&event.item),
        Event::ResponseImageGenerationCallPartialImage(_)
        | Event::ResponseImageGenerationCallCompleted(_) => true,
        Event::ResponseCreated(_)
        | Event::ResponseInProgress(_)
        | Event::ResponseQueued(_)
        | Event::ResponseOutputTextDone(_)
        | Event::ResponseRefusalDone(_)
        | Event::ResponseFunctionCallArgumentsDone(_)
        | Event::ResponseReasoningSummaryTextDone(_)
        | Event::ResponseReasoningTextDone(_)
        | Event::ResponseMCPCallArgumentsDone(_)
        | Event::ResponseCodeInterpreterCallCodeDone(_)
        | Event::ResponseCustomToolCallInputDone(_)
        | Event::ResponseFailed(_)
        | Event::ResponseCompleted(_)
        | Event::ResponseIncomplete(_)
        | Event::ResponseOutputItemDone(_)
        | Event::ResponseContentPartAdded(_)
        | Event::ResponseContentPartDone(_)
        | Event::ResponseFileSearchCallInProgress(_)
        | Event::ResponseFileSearchCallSearching(_)
        | Event::ResponseFileSearchCallCompleted(_)
        | Event::ResponseWebSearchCallInProgress(_)
        | Event::ResponseWebSearchCallSearching(_)
        | Event::ResponseWebSearchCallCompleted(_)
        | Event::ResponseReasoningSummaryPartAdded(_)
        | Event::ResponseReasoningSummaryPartDone(_)
        | Event::ResponseImageGenerationCallGenerating(_)
        | Event::ResponseImageGenerationCallInProgress(_)
        | Event::ResponseMCPCallCompleted(_)
        | Event::ResponseMCPCallFailed(_)
        | Event::ResponseMCPCallInProgress(_)
        | Event::ResponseMCPListToolsCompleted(_)
        | Event::ResponseMCPListToolsFailed(_)
        | Event::ResponseMCPListToolsInProgress(_)
        | Event::ResponseCodeInterpreterCallInProgress(_)
        | Event::ResponseCodeInterpreterCallInterpreting(_)
        | Event::ResponseCodeInterpreterCallCompleted(_)
        | Event::ResponseOutputTextAnnotationAdded(_)
        | Event::ResponseError(_) => false,
    }
}

fn output_item_is_tool_call(item: &rs::OutputItem) -> bool {
    use rs::OutputItem;
    match item {
        OutputItem::FunctionCall(_)
        | OutputItem::CustomToolCall(_)
        | OutputItem::WebSearchCall(_)
        | OutputItem::FileSearchCall(_)
        | OutputItem::ComputerCall(_)
        | OutputItem::ImageGenerationCall(_)
        | OutputItem::CodeInterpreterCall(_)
        | OutputItem::LocalShellCall(_)
        | OutputItem::ShellCall(_)
        | OutputItem::ApplyPatchCall(_)
        | OutputItem::McpCall(_) => true,
        OutputItem::Message(_)
        | OutputItem::Reasoning(_)
        | OutputItem::Compaction(_)
        | OutputItem::McpListTools(_)
        | OutputItem::McpApprovalRequest(_)
        | OutputItem::ShellCallOutput(_)
        | OutputItem::ApplyPatchCallOutput(_) => false,
    }
}

fn message_event_has_content(event: &messages::MessageStreamEvent) -> bool {
    use messages::{ContentBlock, MessageStreamEvent, StreamDelta};
    match event {
        MessageStreamEvent::ContentBlockDelta { delta, .. } => match delta {
            StreamDelta::TextDelta { text } => !text.is_empty(),
            StreamDelta::ThinkingDelta { thinking } => !thinking.is_empty(),
            StreamDelta::InputJsonDelta { partial_json } => !partial_json.is_empty(),
            StreamDelta::SignatureDelta { .. } => false,
        },
        MessageStreamEvent::ContentBlockStart { content_block, .. } => match content_block {
            ContentBlock::ToolUse { .. } => true,
            ContentBlock::Text { text, .. } => !text.is_empty(),
            ContentBlock::Thinking { thinking, .. } => !thinking.is_empty(),
            ContentBlock::Image { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::RedactedThinking { .. } => false,
        },
        MessageStreamEvent::MessageStart { .. }
        | MessageStreamEvent::MessageDelta { .. }
        | MessageStreamEvent::MessageStop
        | MessageStreamEvent::ContentBlockStop { .. }
        | MessageStreamEvent::Ping
        | MessageStreamEvent::Error { .. } => false,
    }
}

fn responses_event_is_error(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent as Event;
    matches!(event, Event::ResponseFailed(_) | Event::ResponseError(_))
}

fn responses_event_is_end(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent as Event;
    matches!(
        event,
        Event::ResponseCompleted(_) | Event::ResponseIncomplete(_)
    )
}

fn message_event_is_error(event: &messages::MessageStreamEvent) -> bool {
    matches!(event, messages::MessageStreamEvent::Error { .. })
}

fn message_event_is_end(event: &messages::MessageStreamEvent) -> bool {
    matches!(event, messages::MessageStreamEvent::MessageStop)
}

pub(crate) fn chat_chunk_class(chunk: &ChatCompletionChunk) -> ItemClass {
    if chat_chunk_has_content(chunk) {
        ItemClass::Content
    } else {
        ItemClass::Other
    }
}

pub(crate) fn responses_event_class(event: &rs::ResponseStreamEvent) -> ItemClass {
    if responses_event_is_error(event) {
        ItemClass::Error
    } else if responses_event_has_content(event) {
        ItemClass::Content
    } else if responses_event_is_end(event) {
        ItemClass::End
    } else {
        ItemClass::Other
    }
}

pub(crate) fn message_event_class(event: &messages::MessageStreamEvent) -> ItemClass {
    if message_event_is_error(event) {
        ItemClass::Error
    } else if message_event_has_content(event) {
        ItemClass::Content
    } else if message_event_is_end(event) {
        ItemClass::End
    } else {
        ItemClass::Other
    }
}
