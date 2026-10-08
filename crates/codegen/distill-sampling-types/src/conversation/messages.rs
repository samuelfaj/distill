use super::*;

/// Marks the last block that can carry one, scanning back past `Thinking`, which the API rejects a breakpoint on.
fn mark_message_cache_breakpoint(msg: &mut crate::messages::Message) -> bool {
    use crate::messages::{CacheControl, ContentBlock, MessageContent};

    match &mut msg.content {
        MessageContent::Blocks(blocks) => {
            for block in blocks.iter_mut().rev() {
                let cache_control = match block {
                    ContentBlock::Text { cache_control, .. }
                    | ContentBlock::ToolResult { cache_control, .. }
                    | ContentBlock::Image { cache_control, .. }
                    | ContentBlock::ToolUse { cache_control, .. } => cache_control,
                    ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::ToolAddition { .. } => {
                        continue;
                    }
                };
                *cache_control = Some(CacheControl::ephemeral());
                return true;
            }
            false
        }
        // Plain text cannot carry a breakpoint, so promote it to block form.
        MessageContent::Text(text) => {
            let text = std::mem::take(text);
            msg.content = MessageContent::Blocks(vec![ContentBlock::Text {
                text,
                cache_control: Some(CacheControl::ephemeral()),
            }]);
            true
        }
    }
}

/// An entry is written only at a breakpoint, so marking the system prompt alone leaves the transcript uncached.
/// The third covers a turn that appends more than the API's 20 block lookback.
/// The leading project-instructions message, when more follows it, is marked only while one of the four slots stays free, so sibling subagents share system, tools and that message on their first request.
/// The fourth slot stays free unless `anchor`: a gateway that turns on automatic caching takes it, and five is rejected outright.
/// With `anchor` it sits on a former tip that advances every [`ANCHOR_ROUNDS`] rounds, so an edit older than the previous tip still reads everything before the anchor.
/// A `one_shot` request is never resent, so it marks nothing, unless `shared_prefix` says its system prompt and leading message repeat.
fn apply_cache_breakpoints(
    system_blocks: &mut [crate::messages::TextBlock],
    messages: &mut [crate::messages::Message],
    leading_project_instructions: bool,
    one_shot: bool,
    shared_prefix: bool,
    anchor: bool,
) {
    use crate::messages::{CacheControl, MessageRole};

    if one_shot && !shared_prefix {
        return;
    }

    if let Some(last) = system_blocks.last_mut() {
        last.cache_control = Some(CacheControl::ephemeral());
    }

    let tip = if one_shot {
        None
    } else {
        (0..messages.len()).rev().find(|&i| {
            messages
                .get_mut(i)
                .is_some_and(mark_message_cache_breakpoint)
        })
    };

    // Where the previous request ended
    // A turn can append several user messages in a row, so skip the whole trailing run rather than a neighbour of the tip
    if let Some(tip) = tip
        && let Some(before_tip) = messages.get(..tip)
        && let Some(prev) = before_tip
            .iter()
            .rposition(|m| matches!(m.role, MessageRole::Assistant))
            .and_then(|assistant| {
                before_tip.get(..assistant).and_then(|before_asst| {
                    before_asst
                        .iter()
                        .rposition(|m| matches!(m.role, MessageRole::User))
                })
            })
        && let Some(msg) = messages.get_mut(prev)
    {
        mark_message_cache_breakpoint(msg);
    }

    if leading_project_instructions && messages.len() > 1 {
        let used = count_cache_breakpoints(system_blocks, messages);
        let already_marked = messages.first().is_some_and(|m| message_breakpoints(m) > 0);
        if used + 1 < MAX_CACHE_BREAKPOINTS
            && !already_marked
            && let Some(first) = messages.first_mut()
        {
            mark_message_cache_breakpoint(first);
        }
    }

    if anchor
        && tip.is_some()
        && count_cache_breakpoints(system_blocks, messages) < MAX_CACHE_BREAKPOINTS
        && let Some(at) = anchor_position(messages)
        && let Some(msg) = messages.get_mut(at)
        && message_breakpoints(msg) == 0
    {
        mark_message_cache_breakpoint(msg);
    }
}

/// Rounds between anchor moves: an edit older than the previous tip re-bills at most about this many rounds.
pub const ANCHOR_ROUNDS: usize = 10;

/// The tip of the request that produced assistant round `j` (the last user message before it), for the
/// largest multiple `j` of [`ANCHOR_ROUNDS`] that is at least two rounds old: the previous tip covers the round before.
/// That request wrote a cache entry exactly there, and every later request names the same position until the anchor moves.
fn anchor_position(messages: &[crate::messages::Message]) -> Option<usize> {
    use crate::messages::MessageRole;

    let assistants: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, MessageRole::Assistant))
        .map(|(i, _)| i)
        .collect();
    let round = assistants.len().checked_sub(2)? / ANCHOR_ROUNDS * ANCHOR_ROUNDS;
    let assistant = *assistants.get(round)?;
    messages
        .get(..assistant)?
        .iter()
        .rposition(|m| matches!(m.role, MessageRole::User))
}

const MAX_CACHE_BREAKPOINTS: usize = 4;

fn message_breakpoints(msg: &crate::messages::Message) -> usize {
    use crate::messages::{ContentBlock, MessageContent};

    match &msg.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter(|block| match block {
                ContentBlock::Text { cache_control, .. }
                | ContentBlock::ToolResult { cache_control, .. }
                | ContentBlock::Image { cache_control, .. }
                | ContentBlock::ToolUse { cache_control, .. } => cache_control.is_some(),
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ToolAddition { .. } => false,
            })
            .count(),
        MessageContent::Text(_) => 0,
    }
}

fn count_cache_breakpoints(
    system_blocks: &[crate::messages::TextBlock],
    messages: &[crate::messages::Message],
) -> usize {
    system_blocks
        .iter()
        .filter(|block| block.cache_control.is_some())
        .count()
        + messages.iter().map(message_breakpoints).sum::<usize>()
}

/// Models that take the effort as a per-message `output_config` marker instead of a top-level one.
/// The id is the bare name, optionally with a dated `-YYYYMMDD` snapshot suffix.
pub fn supports_per_message_effort(model: &str) -> bool {
    is_model(
        model,
        &[
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
        ],
    )
}

/// Models that take a system-role message inside `messages`, so a changed system prompt can follow the cached history
/// instead of rewriting the top-level one. Claude Sonnet 5 takes only the top-level `system`.
pub fn supports_system_messages(model: &str) -> bool {
    is_model(
        model,
        &[
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-opus-5-5",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
        ],
    )
}

/// Models that take mid-conversation tool changes (in beta): a tool declared with `defer_loading` and offered later by a
/// `tool_addition` block in a system-role message. The same models as [`supports_system_messages`].
pub fn supports_tool_changes(model: &str) -> bool {
    supports_system_messages(model)
}

/// `model` is one of `models`, bare or with a dated `-YYYYMMDD` snapshot suffix.
fn is_model(model: &str, models: &[&str]) -> bool {
    models.iter().any(|base| {
        model.strip_prefix(base).is_some_and(|rest| {
            rest.is_empty()
                || rest
                    .strip_prefix('-')
                    .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
        })
    })
}

/// Cache choices only the sending client can make, from the endpoint it talks to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessagesCacheOptions {
    /// Spend the fourth breakpoint on a rolling anchor. Only where nothing in between adds automatic caching,
    /// which takes a slot of its own (a fifth breakpoint is rejected).
    pub anchor: bool,
    /// Give the breakpoints the one-hour lifetime when the request asks for it ([`ConversationRequest::long_cache_ttl`]).
    pub extended_ttl: bool,
    /// Send each system prompt update ([`SyntheticReason::SystemPromptUpdate`]) as a system-role message after the
    /// history it follows, for a model in [`supports_system_messages`], so the cached prefix stays. Off, or where one
    /// cannot sit, the latest update replaces the top-level prompt, as rewriting the head did.
    pub system_messages: bool,
    /// Declare [`ConversationRequest::deferred_tools`] and every tool a [`SyntheticReason::ToolAddition`] item names
    /// with `defer_loading`, and offer each joined tool with a `tool_addition` block where it joined, for a model in
    /// [`supports_tool_changes`], so a tool family joining later keeps the cached prefix. Off, or where an addition
    /// cannot sit, the request sends the tools in effect, as before.
    pub deferred_tools: bool,
}

/// Opens a system prompt update sent as a system-role message: the model must drop the instructions it replaces.
pub const SYSTEM_PROMPT_UPDATE_PREAMBLE: &str = "The system prompt has changed. The instructions below replace, in full, the system prompt this conversation started with and any earlier system prompt update. Follow them from here on.";

/// Where a system message for a history item that came after `at` messages can sit: a system message with content must
/// follow a user message and precede an assistant one or end the request, so it goes after the user turn that follows
/// the item, ahead of the next assistant message. `None` when no user message comes right before that place.
fn system_message_position(messages: &[crate::messages::Message], at: usize) -> Option<usize> {
    use crate::messages::MessageRole;

    let pos = messages
        .iter()
        .skip(at)
        .position(|m| matches!(m.role, MessageRole::Assistant))
        .map_or(messages.len(), |i| at + i);
    pos.checked_sub(1)
        .and_then(|before| messages.get(before))
        .filter(|m| matches!(m.role, MessageRole::User))
        .map(|_| pos)
}

/// The tools a request declares deferred, and where each joined tool is offered.
struct DeferredToolPlan {
    /// Names sent with `defer_loading`.
    deferred: std::collections::BTreeSet<String>,
    /// Each `tool_addition` message: its position among the built messages and the names it offers.
    additions: Vec<(usize, Vec<String>)>,
}

/// Declares [`ConversationRequest::deferred_tools`] and every offered tool a [`SyntheticReason::ToolAddition`] item
/// names deferred, and offers each of the latter at its first item. A named tool the request does not offer (not judged
/// needed again after a resume, or gone) gets no addition, so the model never sees it.
/// `None` sends the tools in effect instead: nothing to defer, an addition with no place a system message can sit, or
/// no tool offered from the start (with every tool deferred, the first one offered would change the prompt's head).
fn deferred_tool_plan(
    req: &ConversationRequest,
    tool_additions: &[(usize, &str)],
    messages: &[crate::messages::Message],
) -> Option<DeferredToolPlan> {
    let offered: std::collections::BTreeSet<&str> =
        req.tools.iter().map(|t| t.name.as_str()).collect();
    let mut deferred: std::collections::BTreeSet<String> = req
        .deferred_tools
        .iter()
        .filter(|t| !offered.contains(t.name.as_str()))
        .map(|t| t.name.clone())
        .collect();
    let mut additions = Vec::new();
    for &(at, names) in tool_additions {
        let mut joined = Vec::new();
        for name in names.lines().map(str::trim) {
            if offered.contains(name) && deferred.insert(name.to_owned()) {
                joined.push(name.to_owned());
            }
        }
        if !joined.is_empty() {
            additions.push((system_message_position(messages, at)?, joined));
        }
    }
    if deferred.is_empty() || req.tools.iter().all(|t| deferred.contains(&t.name)) {
        return None;
    }
    Some(DeferredToolPlan {
        deferred,
        additions,
    })
}

/// A system-role message that offers `names`, each declared deferred in `tools`.
fn tool_addition_message(names: &[String]) -> crate::messages::Message {
    use crate::messages::{ContentBlock, Message, MessageContent, MessageRole, ToolChangeTarget};

    Message {
        role: MessageRole::System,
        content: MessageContent::Blocks(
            names
                .iter()
                .map(|name| ContentBlock::ToolAddition {
                    tool: ToolChangeTarget::ToolReference { name: name.clone() },
                })
                .collect(),
        ),
        output_config: None,
    }
}

pub fn build_messages_request(req: &ConversationRequest) -> crate::messages::MessagesRequest {
    build_messages_request_with(req, MessagesCacheOptions::default())
}

pub fn build_messages_request_with(
    req: &ConversationRequest,
    cache: MessagesCacheOptions,
) -> crate::messages::MessagesRequest {
    use crate::messages::{
        ContentBlock, ImageSource, Message, MessageContent, MessageRole, MessagesRequest,
        OutputConfig, SystemParam, TextBlock, ToolChoiceParam, ToolParam, ToolResultContent,
    };

    let mut system_blocks: Vec<TextBlock> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    // Each system prompt update with the number of messages built before it.
    let mut updates: Vec<(usize, &str)> = Vec::new();
    // Each tool addition (its names, one per line) with the number of messages built before it.
    let mut tool_additions: Vec<(usize, &str)> = Vec::new();
    let mut pending_assistant: Vec<ContentBlock> = Vec::new();
    let mut pending_tool_results: Vec<ContentBlock> = Vec::new();

    let sanitize_tool_call_id = |id: &str| -> String {
        id.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };

    let content_parts_to_anthropic_blocks = |parts: &[ContentPart]| -> Vec<ContentBlock> {
        parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => ContentBlock::Text {
                    text: text.as_ref().to_owned(),
                    cache_control: None,
                },
                ContentPart::Image { url } => {
                    if url.starts_with("data:") {
                        if let Some((header, data)) = url.split_once(',') {
                            let media_type = header
                                .strip_prefix("data:")
                                .and_then(|h| h.strip_suffix(";base64"))
                                .unwrap_or("image/png")
                                .to_string();
                            ContentBlock::Image {
                                source: ImageSource::Base64 {
                                    media_type,
                                    data: data.to_string(),
                                },
                                cache_control: None,
                            }
                        } else {
                            // Malformed data URI, treat as text
                            ContentBlock::Text {
                                text: format!("[invalid image: {}]", url),
                                cache_control: None,
                            }
                        }
                    } else if url.starts_with("http://") || url.starts_with("https://") {
                        ContentBlock::Image {
                            source: ImageSource::Url {
                                url: url.as_ref().to_owned(),
                            },
                            cache_control: None,
                        }
                    } else {
                        // Unknown format, treat as text
                        ContentBlock::Text {
                            text: format!("[image: {}]", url),
                            cache_control: None,
                        }
                    }
                }
            })
            .collect()
    };

    let flush_assistant = |pending: &mut Vec<ContentBlock>, msgs: &mut Vec<Message>| {
        if !pending.is_empty() {
            msgs.push(Message {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(pending.clone()),
                output_config: None,
            });
            pending.clear();
        }
    };

    let flush_tool_results = |pending: &mut Vec<ContentBlock>, msgs: &mut Vec<Message>| {
        if !pending.is_empty() {
            msgs.push(Message {
                role: MessageRole::User,
                content: MessageContent::Blocks(pending.clone()),
                output_config: None,
            });
            pending.clear();
        }
    };

    for item in &req.items {
        match item {
            ConversationItem::System(s) if item.is_system_prompt_update() => {
                flush_assistant(&mut pending_assistant, &mut messages);
                flush_tool_results(&mut pending_tool_results, &mut messages);
                updates.push((messages.len(), s.content.as_ref()));
            }
            ConversationItem::System(s) if item.is_tool_addition() => {
                flush_assistant(&mut pending_assistant, &mut messages);
                flush_tool_results(&mut pending_tool_results, &mut messages);
                tool_additions.push((messages.len(), s.content.as_ref()));
            }
            ConversationItem::System(s) => {
                flush_assistant(&mut pending_assistant, &mut messages);
                flush_tool_results(&mut pending_tool_results, &mut messages);
                system_blocks.push(TextBlock {
                    r#type: "text".to_string(),
                    text: s.content.as_ref().to_owned(),
                    cache_control: None,
                });
            }
            ConversationItem::User(u) => {
                flush_assistant(&mut pending_assistant, &mut messages);
                flush_tool_results(&mut pending_tool_results, &mut messages);
                let blocks = content_parts_to_anthropic_blocks(&u.content);
                messages.push(Message {
                    role: MessageRole::User,
                    content: MessageContent::Blocks(blocks),
                    output_config: None,
                });
            }
            ConversationItem::Assistant(a) => {
                flush_tool_results(&mut pending_tool_results, &mut messages);

                if !a.content.is_empty() {
                    pending_assistant.push(ContentBlock::Text {
                        text: a.content.as_ref().to_owned(),
                        cache_control: None,
                    });
                }

                for tc in &a.tool_calls {
                    let input =
                        serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));
                    pending_assistant.push(ContentBlock::ToolUse {
                        id: sanitize_tool_call_id(&tc.id),
                        name: tc.name.clone(),
                        input,
                        cache_control: None,
                    });
                }
            }
            ConversationItem::ToolResult(t) => {
                flush_assistant(&mut pending_assistant, &mut messages);
                let content = if t.images.is_empty() {
                    ToolResultContent::Text(t.content.as_ref().to_owned())
                } else {
                    let mut blocks = vec![ContentBlock::Text {
                        text: t.content.as_ref().to_owned(),
                        cache_control: None,
                    }];
                    for img in &t.images {
                        if let ContentPart::Image { url } = img {
                            let source = if let Some(rest) = url.strip_prefix("data:") {
                                if let Some((media_type, data)) = rest.split_once(";base64,") {
                                    ImageSource::Base64 {
                                        media_type: media_type.to_string(),
                                        data: data.to_string(),
                                    }
                                } else {
                                    ImageSource::Url {
                                        url: url.as_ref().to_owned(),
                                    }
                                }
                            } else {
                                ImageSource::Url {
                                    url: url.as_ref().to_owned(),
                                }
                            };
                            blocks.push(ContentBlock::Image {
                                source,
                                cache_control: None,
                            });
                        }
                    }
                    ToolResultContent::Blocks(blocks)
                };
                pending_tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: sanitize_tool_call_id(&t.tool_call_id),
                    content,
                    cache_control: None,
                });
            }
            // No native equivalent, so emit synthetic text to retain context.
            ConversationItem::BackendToolCall(b) => {
                flush_tool_results(&mut pending_tool_results, &mut messages);
                pending_assistant.push(ContentBlock::Text {
                    text: b.text_summary(),
                    cache_control: None,
                });
            }
            // `tco_*` blobs carry only `signature`; real reasoning sets `thinking`
            ConversationItem::Reasoning(r) => {
                flush_tool_results(&mut pending_tool_results, &mut messages);
                let thinking = reasoning_item_text(r);
                let signature = r
                    .encrypted_content
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_default();
                if !thinking.is_empty() || !signature.is_empty() {
                    pending_assistant.push(ContentBlock::Thinking {
                        thinking,
                        signature,
                    });
                }
            }
        }
    }

    flush_assistant(&mut pending_assistant, &mut messages);
    flush_tool_results(&mut pending_tool_results, &mut messages);

    // A system message with content must follow a user message and precede an assistant one or end the request,
    // so each update sits after the user turn that follows it, ahead of the next assistant message.
    let update_positions: Vec<usize> =
        if cache.system_messages && req.model.as_deref().is_some_and(supports_system_messages) {
            updates
                .iter()
                .map(|&(at, _)| system_message_position(&messages, at))
                .collect::<Option<Vec<usize>>>()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    let in_history = !updates.is_empty() && update_positions.len() == updates.len();
    // Otherwise the latest update replaces the top-level prompt, the request a rewritten head sent.
    if !in_history && let Some(&(_, latest)) = updates.last() {
        match system_blocks.first_mut() {
            Some(head) => head.text = latest.to_owned(),
            None => system_blocks.push(TextBlock {
                r#type: "text".to_string(),
                text: latest.to_owned(),
                cache_control: None,
            }),
        }
    }
    // `None` sends the tools in effect and no addition, the request before deferred tools.
    let deferred =
        if cache.deferred_tools && req.model.as_deref().is_some_and(supports_tool_changes) {
            deferred_tool_plan(req, &tool_additions, &messages)
        } else {
            None
        };

    // The leading project-instructions item becomes `messages[0]` because the system item precedes it.
    let leading_project_instructions = matches!(
        (req.items.first(), req.items.get(1)),
        (
            Some(ConversationItem::System(_)),
            Some(ConversationItem::User(u)),
        ) if u.synthetic_reason == SyntheticReason::ProjectInstructions
    );
    apply_cache_breakpoints(
        &mut system_blocks,
        &mut messages,
        leading_project_instructions,
        req.one_shot,
        req.shared_prefix,
        cache.anchor,
    );

    // Inserted after the breakpoints, which never land on a system message; in reverse so earlier positions hold.
    // At one position the tool additions come first (the stable sort keeps them ahead of the updates).
    let mut system_messages: Vec<(usize, Message)> = Vec::new();
    if let Some(plan) = &deferred {
        for (pos, names) in &plan.additions {
            system_messages.push((*pos, tool_addition_message(names)));
        }
    }
    if in_history {
        for (&pos, &(_, text)) in update_positions.iter().zip(&updates) {
            system_messages.push((
                pos,
                Message {
                    role: MessageRole::System,
                    content: MessageContent::Text(format!(
                        "{SYSTEM_PROMPT_UPDATE_PREAMBLE}\n\n{text}"
                    )),
                    output_config: None,
                },
            ));
        }
    }
    system_messages.sort_by_key(|(pos, _)| *pos);
    for (pos, message) in system_messages.into_iter().rev() {
        messages.insert(pos, message);
    }

    let system: Option<SystemParam> = if system_blocks.is_empty() {
        None
    } else if let [block] = system_blocks.as_slice()
        && block.cache_control.is_none()
    {
        Some(SystemParam::Text(block.text.clone()))
    } else {
        Some(SystemParam::Blocks(system_blocks))
    };

    let tools: Option<Vec<ToolParam>> = if req.tools.is_empty() {
        None
    } else if let Some(plan) = &deferred {
        // Offered and deferred tools in one array, by name: a tool that joins keeps its place and its bytes.
        let mut seen = std::collections::BTreeSet::new();
        let mut tools: Vec<ToolParam> = req
            .tools
            .iter()
            .chain(&req.deferred_tools)
            .filter(|t| seen.insert(t.name.clone()))
            .map(|t| ToolParam {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
                defer_loading: plan.deferred.contains(&t.name).then_some(true),
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Some(tools)
    } else {
        Some(
            req.tools
                .iter()
                .map(|t| ToolParam {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.parameters.clone(),
                    defer_loading: None,
                })
                .collect(),
        )
    };

    let tool_choice: Option<ToolChoiceParam> = req.tool_choice.as_ref().map(|tc| match tc {
        ConversationToolChoice::Auto => ToolChoiceParam::Auto,
        ConversationToolChoice::Required => ToolChoiceParam::Any,
        ConversationToolChoice::Function(name) => ToolChoiceParam::Tool { name: name.clone() },
        ConversationToolChoice::None => ToolChoiceParam::Auto, // ToolChoiceParam has no none variant, so fall back to the default
    });

    let mut effort = req
        .reasoning_effort
        .and_then(|e| e.to_messages_api())
        .map(|s| s.to_string());

    // A wire schema here suppresses tool calls, so the agent routes structured output through the StructuredOutput tool instead
    let format = req
        .json_schema
        .as_ref()
        .map(|schema| crate::messages::OutputFormat::JsonSchema {
            schema: schema.clone(),
        });

    // thinking is driven by reasoning_effort only, not by json_schema.
    let thinking = effort
        .as_ref()
        .map(|_| crate::messages::ThinkingConfig::Adaptive {
            display: Some(crate::messages::ThinkingDisplay::Summarized),
        });

    // A top-level effort change restarts the prompt cache and a marker does not, so models that take one get the effort as a system-role marker after the last assistant message.
    // A marker after the final user message would not apply to this request, hence the last-role check (past a trailing system prompt update).
    let per_message_effort = effort.is_some()
        && req
            .model
            .as_deref()
            .is_some_and(supports_per_message_effort)
        && messages
            .iter()
            .rfind(|m| !matches!(m.role, MessageRole::System))
            .is_some_and(|m| matches!(m.role, MessageRole::User));
    let output_config = if per_message_effort {
        let at = messages
            .iter()
            .rposition(|m| matches!(m.role, MessageRole::Assistant))
            .map_or(0, |i| i + 1);
        messages.insert(
            at,
            Message {
                role: MessageRole::System,
                content: MessageContent::Blocks(Vec::new()),
                output_config: Some(OutputConfig {
                    effort: effort.take(),
                    format: None,
                }),
            },
        );
        format.map(|format| OutputConfig {
            effort: None,
            format: Some(format),
        })
    } else if effort.is_some() || format.is_some() {
        Some(OutputConfig { effort, format })
    } else {
        None
    };

    let mut request = MessagesRequest {
        model: req.model.clone().unwrap_or_default(),
        messages,
        max_tokens: req.max_output_tokens.unwrap_or(0),
        system,
        tools,
        tool_choice,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: None,
        stream: None, // The caller sets this
        stop_sequences: None,
        thinking,
        output_config,
        metadata: None,
    };
    if cache.extended_ttl && req.long_cache_ttl {
        request.set_cache_ttl(Some(crate::messages::EXTENDED_CACHE_TTL));
    }
    request
}

/// `Thinking` is dropped because this `From` returns a single item; the streaming consumer emits the sibling `Reasoning` item instead.
impl From<crate::messages::MessagesResponse> for ConversationItem {
    fn from(resp: crate::messages::MessagesResponse) -> Self {
        use crate::messages::ContentBlock;

        let mut content = String::new();
        let mut tool_calls = Vec::new();

        for block in resp.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&text);
                }
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    tool_calls.push(ToolCall {
                        id: Arc::<str>::from(id),
                        name,
                        arguments: Arc::<str>::from(
                            serde_json::to_string(&input).unwrap_or_default(),
                        ),
                    });
                }
                // Thinking is dropped; see the doc comment above
                ContentBlock::Thinking { .. } => {}
                _ => {} // Image and ToolResult are not expected in assistant responses
            }
        }

        ConversationItem::Assistant(AssistantItem {
            content: Arc::<str>::from(content),
            tool_calls,
            model_id: Some(resp.model),
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }
}
