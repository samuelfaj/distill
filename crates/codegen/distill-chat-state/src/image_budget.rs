// Modified for Distill by Samuel Fajreldines, 2026.
//! Cache-aware byte budgeting for inline images on owned request histories.

use distill_sampling_types::{ContentPart, ConversationItem};

/// Replaces an inline image evicted to keep the request body under the proxy's 50 MB limit.
/// Phrased so the model treats the image as gone — a silent strip otherwise induces hallucination.
const IMAGE_COMPACT_PLACEHOLDER: &str = "[An earlier image was removed to keep the request within its size limit and is no longer visible. Do not describe or reason about its contents from memory; ask the user to re-share it if you need to see it again.]";

/// Appended to a tool result when any of its images are evicted. Tool-result
/// image arrays cannot carry model-visible text, so the omission belongs in the
/// result content itself.
const TOOL_IMAGE_COMPACT_NOTE: &str = "[One or more images from this tool result were removed to keep the request within its size limit and are no longer visible. Do not describe or reason about their contents from memory.]";

/// Hard request-body ceiling enforced by the inference proxy (nginx `proxy-body-size`).
/// Larger bodies are rejected with HTTP 413 or a connection reset. Inline image `data:` URLs dominate.
const MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;

/// Evict old images once the serialized body reaches this size (3 MB below the hard ceiling).
/// Headroom covers uncounted tool definitions and the request envelope.
/// Below the trigger every image stays so the KV-cache prefix remains byte-stable.
pub const IMAGE_COMPACT_TRIGGER_BYTES: usize = MAX_REQUEST_BYTES - 3 * 1024 * 1024;

/// Low-water mark that eviction reclaims down to once it fires (hysteresis).
/// Clearing only the trigger would re-cross and re-bust the KV cache every turn.
/// Dropping to half the hard limit rewrites the prefix once, then stays cache-warm.
pub const IMAGE_COMPACT_RECLAIM_TARGET_BYTES: usize = MAX_REQUEST_BYTES / 2;

// Hysteresis invariant: eviction is gated at the trigger but reclaims to a strictly lower mark.
// One batch eviction buys many cache-warm turns instead of re-busting the prompt cache every turn.
// Enforced at compile time so the two constants cannot drift together.
const _: () = assert!(IMAGE_COMPACT_RECLAIM_TARGET_BYTES < IMAGE_COMPACT_TRIGGER_BYTES);

/// An [`std::io::Write`] sink that counts bytes instead of storing them. Lets
/// us measure a `serde_json` encoding's length without allocating the full
/// (potentially tens-of-MB) output buffer.
#[derive(Default)]
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Exact JSON-serialized byte length of any value, measured through a
/// [`ByteCounter`] so no encoded buffer is allocated. JSON quoting and string
/// escaping are captured precisely (not estimated from field lengths).
fn serialized_json_bytes<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = ByteCounter::default();
    if let Err(err) = serde_json::to_writer(&mut counter, value) {
        // Serializing in-memory state to a byte sink is infallible in
        // practice; if it ever fails, fall back to the bytes counted so far
        // (a lower bound) rather than forcing a needless compaction.
        tracing::warn!(%err, "failed to measure serialized size");
    }
    counter.0
}

fn inline_image_count(conversation: &[ConversationItem]) -> usize {
    conversation
        .iter()
        .map(|item| match item {
            ConversationItem::User(user) => user
                .content
                .iter()
                .filter(|part| matches!(part, ContentPart::Image { .. }))
                .count(),
            ConversationItem::ToolResult(tool_result) => tool_result
                .images
                .iter()
                .filter(|part| matches!(part, ContentPart::Image { .. }))
                .count(),
            ConversationItem::System(_)
            | ConversationItem::Assistant(_)
            | ConversationItem::BackendToolCall(_)
            | ConversationItem::Reasoning(_) => 0,
        })
        .sum()
}

/// Exact image-budget accounting for an owned request history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageBudgetOutcome {
    /// Exact serialized conversation bytes before any eviction.
    pub body_bytes: usize,
    /// Exact serialized conversation bytes after all eviction mutations.
    pub body_bytes_after: usize,
    /// Number of user and tool-result images before eviction.
    pub inline_images: usize,
    /// Whether the pre-eviction body reached the configured trigger.
    pub needs_image_compaction: bool,
    /// Number of images evicted from the request history.
    pub evicted: usize,
}

/// Owned request history paired with its image-budget outcome.
#[derive(Clone, Debug)]
pub struct BudgetedConversation {
    /// Request history after any required image eviction.
    pub items: Vec<ConversationItem>,
    /// Byte and eviction accounting for the transformation.
    pub outcome: ImageBudgetOutcome,
}

/// Applies the production 47 MiB trigger and 25 MiB reclaim target.
#[must_use]
pub fn apply_image_budget(items: Vec<ConversationItem>) -> BudgetedConversation {
    apply_image_budget_with_limits(
        items,
        IMAGE_COMPACT_TRIGGER_BYTES,
        IMAGE_COMPACT_RECLAIM_TARGET_BYTES,
    )
}

/// Applies an explicit high-water trigger and low-water reclaim target.
/// Callers that add request fields outside the conversation can subtract those bytes from both limits.
#[must_use]
pub fn apply_image_budget_with_limits(
    mut items: Vec<ConversationItem>,
    trigger_bytes: usize,
    reclaim_target_bytes: usize,
) -> BudgetedConversation {
    let body_bytes = conversation_body_bytes(&items);
    let inline_images = inline_image_count(&items);
    let needs_image_compaction = body_bytes >= trigger_bytes;
    let mut evicted = 0;
    if needs_image_compaction && body_bytes > reclaim_target_bytes {
        evicted = evict_images_to_budget(&mut items, body_bytes, reclaim_target_bytes);
    }
    let body_bytes_after = if evicted == 0 {
        body_bytes
    } else {
        conversation_body_bytes(&items)
    };
    BudgetedConversation {
        items,
        outcome: ImageBudgetOutcome {
            body_bytes,
            body_bytes_after,
            inline_images,
            needs_image_compaction,
            evicted,
        },
    }
}

fn encoded_url_bytes(url: &str) -> usize {
    let Some((header, base64_tail)) = url.split_once(",") else {
        return serialized_json_bytes(url).saturating_sub(2);
    };
    if header.starts_with("data:") && header.ends_with(";base64") {
        serialized_json_bytes(header).saturating_sub(2) + 1 + base64_tail.len()
    } else {
        serialized_json_bytes(url).saturating_sub(2)
    }
}

const IMAGE_PART_FRAME_BYTES: usize = r#"{"type":"image","url":""}"#.len();

fn image_part_bytes(part: &ContentPart) -> usize {
    match part {
        ContentPart::Image { url } => IMAGE_PART_FRAME_BYTES + encoded_url_bytes(url),
        ContentPart::Text { .. } => unreachable!("image discovery must return only images"),
    }
}

/// Exact serialized size of the conversation body without scanning base64 image tails.
/// Blanks image URLs before serialization, then adds back each URL's exact escaped contribution.
/// For a base64 data URI, only the header is escape-scanned; the base64 tail cannot need JSON escaping.
fn conversation_body_bytes(conversation: &[ConversationItem]) -> usize {
    let mut blanked = conversation.to_vec();
    let mut image_url_bytes = 0usize;
    for item in &mut blanked {
        match item {
            ConversationItem::User(user) => {
                blank_image_urls(&mut user.content, &mut image_url_bytes);
            }
            ConversationItem::ToolResult(tool_result) => {
                blank_image_urls(&mut tool_result.images, &mut image_url_bytes);
            }
            ConversationItem::System(_)
            | ConversationItem::Assistant(_)
            | ConversationItem::BackendToolCall(_)
            | ConversationItem::Reasoning(_) => {}
        }
    }
    serialized_json_bytes(&blanked) + image_url_bytes
}

fn blank_image_urls(parts: &mut [ContentPart], image_url_bytes: &mut usize) {
    for part in parts {
        if let ContentPart::Image { url } = part {
            *image_url_bytes += encoded_url_bytes(url);
            *url = std::sync::Arc::<str>::from("");
        }
    }
}

#[derive(Clone, Copy)]
enum ImageLocation {
    User { item: usize, part: usize },
    ToolResult { item: usize },
}

fn evict_images_to_budget(
    conversation: &mut [ConversationItem],
    current_bytes: usize,
    target_bytes: usize,
) -> usize {
    let placeholder = ContentPart::Text {
        text: std::sync::Arc::<str>::from(IMAGE_COMPACT_PLACEHOLDER),
    };
    let placeholder_bytes = serialized_json_bytes(&placeholder);
    let tool_note_suffix = format!("\n\n{TOOL_IMAGE_COMPACT_NOTE}");
    let tool_note_bytes = serialized_json_bytes(&tool_note_suffix).saturating_sub(2);
    let mut images = Vec::new();
    for (item, conversation_item) in conversation.iter().enumerate() {
        match conversation_item {
            ConversationItem::User(user) => {
                for (part, content_part) in user.content.iter().enumerate() {
                    if let ContentPart::Image { .. } = content_part {
                        images.push((
                            ImageLocation::User { item, part },
                            image_part_bytes(content_part),
                        ));
                    }
                }
            }
            ConversationItem::ToolResult(tool_result) => {
                for content_part in &tool_result.images {
                    if let ContentPart::Image { .. } = content_part {
                        images.push((
                            ImageLocation::ToolResult { item },
                            image_part_bytes(content_part),
                        ));
                    }
                }
            }
            ConversationItem::System(_)
            | ConversationItem::Assistant(_)
            | ConversationItem::BackendToolCall(_)
            | ConversationItem::Reasoning(_) => {}
        }
    }

    let mut running = current_bytes;
    let mut evicted = 0;
    for (location, image_bytes) in images {
        if running <= target_bytes {
            break;
        }
        match location {
            ImageLocation::User { item, part } => {
                let Some(ConversationItem::User(user)) = conversation.get_mut(item) else {
                    continue;
                };
                let Some(slot) = user.content.get_mut(part) else {
                    continue;
                };
                *slot = placeholder.clone();
                running = running.saturating_sub(image_bytes.saturating_sub(placeholder_bytes));
            }
            ImageLocation::ToolResult { item } => {
                let Some(ConversationItem::ToolResult(tool_result)) = conversation.get_mut(item)
                else {
                    continue;
                };
                let image_index = tool_result
                    .images
                    .iter()
                    .position(|part| matches!(part, ContentPart::Image { .. }))
                    .unwrap_or_else(|| unreachable!("discovered tool image must remain present"));
                tool_result.images.remove(image_index);
                running = running
                    .saturating_sub(image_bytes + usize::from(!tool_result.images.is_empty()));
                if !tool_result.content.contains(TOOL_IMAGE_COMPACT_NOTE) {
                    tool_result.content =
                        format!("{}\n\n{TOOL_IMAGE_COMPACT_NOTE}", tool_result.content).into();
                    running = running.saturating_add(tool_note_bytes);
                }
            }
        }
        evicted += 1;
    }
    evicted
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_sampling_types::ToolCall;

    fn image(url: impl Into<String>) -> ContentPart {
        ContentPart::Image {
            url: url.into().into(),
        }
    }

    fn data_image(byte: char, bytes: usize) -> ContentPart {
        let prefix = "data:image/png;base64,";
        image(format!(
            "{prefix}{}",
            byte.to_string().repeat(bytes - prefix.len())
        ))
    }

    fn mixed_history() -> Vec<ConversationItem> {
        vec![
            ConversationItem::user_with_parts(vec![
                ContentPart::Text {
                    text: "old user".into(),
                },
                data_image('U', 500),
            ]),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: r#"{"target_file":"image.png"}"#.into(),
            }]),
            ConversationItem::tool_result_with_images(
                "call-1",
                "tool text",
                vec![
                    ContentPart::Text {
                        text: "metadata".into(),
                    },
                    data_image('A', 500),
                    data_image('B', 500),
                    data_image('C', 500),
                ],
            ),
            ConversationItem::user_with_parts(vec![data_image('N', 500)]),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call-2".into(),
                name: "read_file".into(),
                arguments: r#"{"target_file":"new.png"}"#.into(),
            }]),
            ConversationItem::tool_result_with_images(
                "call-2",
                "new tool text",
                vec![data_image('Z', 500)],
            ),
        ]
    }

    fn expected_after_three_evictions() -> Vec<ConversationItem> {
        let mut expected = mixed_history();
        let Some(ConversationItem::User(user)) = expected.get_mut(0) else {
            panic!("expected user item: {expected:?}");
        };
        let Some(slot) = user.content.get_mut(1) else {
            panic!("expected image part: {:?}", user.content);
        };
        *slot = ContentPart::Text {
            text: IMAGE_COMPACT_PLACEHOLDER.into(),
        };
        let Some(ConversationItem::ToolResult(tool_result)) = expected.get_mut(2) else {
            panic!("expected tool result: {expected:?}");
        };
        tool_result.images.remove(1);
        tool_result.images.remove(1);
        tool_result.content = format!("tool text\n\n{TOOL_IMAGE_COMPACT_NOTE}").into();
        expected
    }

    #[test]
    fn mixed_escaped_urls_measure_exactly() {
        let history = vec![
            ConversationItem::user_with_parts(vec![image("https://example.com/a\"b\\c\n")]),
            ConversationItem::tool_result_with_images(
                "call-1",
                "tool",
                vec![
                    image("https://example.com/d\"e\\f\t"),
                    image("data:image/\"png\\escaped;base64,AAAA"),
                ],
            ),
        ];
        assert_eq!(
            conversation_body_bytes(&history),
            serde_json::to_vec(&history).unwrap().len()
        );
    }

    #[test]
    fn below_trigger_preserves_complete_history() {
        let history = mixed_history();
        let expected = serde_json::to_value(&history).unwrap();
        let budgeted = apply_image_budget(history);
        assert_eq!(
            budgeted.outcome,
            ImageBudgetOutcome {
                body_bytes: budgeted.outcome.body_bytes,
                body_bytes_after: budgeted.outcome.body_bytes,
                inline_images: 6,
                needs_image_compaction: false,
                evicted: 0,
            }
        );
        assert_eq!(serde_json::to_value(budgeted.items).unwrap(), expected);
        assert_eq!(IMAGE_COMPACT_TRIGGER_BYTES, 47 * 1024 * 1024);
        assert_eq!(IMAGE_COMPACT_RECLAIM_TARGET_BYTES, 25 * 1024 * 1024);
    }

    #[test]
    fn partial_tool_result_eviction_is_exact_and_ordered() {
        let expected = expected_after_three_evictions();
        let target = conversation_body_bytes(&expected);
        let budgeted = apply_image_budget_with_limits(mixed_history(), 1, target);
        assert_eq!(
            serde_json::to_value(&budgeted.items).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(
            budgeted.outcome,
            ImageBudgetOutcome {
                body_bytes: conversation_body_bytes(&mixed_history()),
                body_bytes_after: target,
                inline_images: 6,
                needs_image_compaction: true,
                evicted: 3,
            }
        );
        assert_eq!(
            budgeted.outcome.body_bytes_after,
            serde_json::to_vec(&budgeted.items).unwrap().len()
        );
        let Some(ConversationItem::ToolResult(tool_result)) = budgeted.items.get(2) else {
            panic!("expected tool result: {:?}", budgeted.items);
        };
        assert_eq!(tool_result.tool_call_id, "call-1");
        assert_eq!(
            tool_result.content.as_ref(),
            format!("tool text\n\n{TOOL_IMAGE_COMPACT_NOTE}")
        );
        assert_eq!(
            tool_result.content.matches(TOOL_IMAGE_COMPACT_NOTE).count(),
            1
        );
        assert!(
            matches!(tool_result.images.as_slice(), [ContentPart::Text { text }, ContentPart::Image { url }] if text.as_ref() == "metadata" && url.contains('C'))
        );
        assert!(
            matches!(budgeted.items.get(3), Some(ConversationItem::User(user)) if matches!(user.content.first(), Some(ContentPart::Image { url }) if url.contains('N')))
        );
        assert!(
            matches!(budgeted.items.get(5), Some(ConversationItem::ToolResult(result)) if matches!(result.images.first(), Some(ContentPart::Image { url }) if url.contains('Z')))
        );
    }

    #[test]
    fn final_tool_image_removal_reports_exact_omitted_field_size() {
        let history = vec![ConversationItem::tool_result_with_images(
            "call-1",
            "tool",
            vec![data_image('A', 500)],
        )];
        let expected = vec![ConversationItem::tool_result(
            "call-1",
            format!("tool\n\n{TOOL_IMAGE_COMPACT_NOTE}"),
        )];
        let target = conversation_body_bytes(&expected);
        let budgeted = apply_image_budget_with_limits(history, 1, target);
        assert_eq!(
            serde_json::to_value(&budgeted.items).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(budgeted.outcome.evicted, 1);
        assert_eq!(budgeted.outcome.body_bytes_after, target);
        assert_eq!(target, serde_json::to_vec(&budgeted.items).unwrap().len());
    }

    #[test]
    fn production_threshold_reclaims_a_batch_to_low_water_mark() {
        let image_bytes = 2 * 1024 * 1024;
        let image_count = IMAGE_COMPACT_TRIGGER_BYTES / image_bytes + 2;
        let history = (0..image_count)
            .map(|index| {
                ConversationItem::user_with_parts(vec![data_image(
                    char::from(b'A' + (index % 26) as u8),
                    image_bytes,
                )])
            })
            .collect();
        let budgeted = apply_image_budget(history);

        assert!(budgeted.outcome.body_bytes >= IMAGE_COMPACT_TRIGGER_BYTES);
        assert!(budgeted.outcome.body_bytes_after <= IMAGE_COMPACT_RECLAIM_TARGET_BYTES);
        assert!(budgeted.outcome.evicted > image_count / 4);
        assert!(matches!(
            budgeted.items.last(),
            Some(ConversationItem::User(user))
                if matches!(user.content.first(), Some(ContentPart::Image { .. }))
        ));
        assert_eq!(
            budgeted.outcome.body_bytes_after,
            serde_json::to_vec(&budgeted.items).unwrap().len()
        );
    }

    #[test]
    fn trigger_and_reclaim_boundaries_are_inclusive() {
        let history = vec![ConversationItem::user_with_parts(vec![data_image(
            'A', 500,
        )])];
        let bytes = conversation_body_bytes(&history);

        let at_trigger = apply_image_budget_with_limits(history.clone(), bytes, bytes);
        assert!(at_trigger.outcome.needs_image_compaction);
        assert_eq!(at_trigger.outcome.evicted, 0);
        assert_eq!(
            serde_json::to_value(at_trigger.items).unwrap(),
            serde_json::to_value(&history).unwrap()
        );

        let below_trigger = apply_image_budget_with_limits(history.clone(), bytes + 1, 0);
        assert!(!below_trigger.outcome.needs_image_compaction);
        assert_eq!(below_trigger.outcome.evicted, 0);
        assert_eq!(
            serde_json::to_value(below_trigger.items).unwrap(),
            serde_json::to_value(history).unwrap()
        );
    }
}
