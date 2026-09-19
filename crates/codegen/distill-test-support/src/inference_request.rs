//! The three inference endpoints, a request as the mock received it, and the readers over its body
//! that work on all three formats.
use crate::conversation::{ConversationId, ConversationKey, ConversationTracker};
use axum::http::HeaderMap;
use serde_json::Value;
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
const TURN_INDEX_HEADER: &str = "x-grok-turn-idx";
const REQUEST_ID_HEADER: &str = "x-grok-req-id";
const SESSION_ID_HEADER: &str = "x-grok-session-id";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InferenceEndpoint {
    ChatCompletions,
    Responses,
    Messages,
}
impl InferenceEndpoint {
    pub(crate) fn path(self) -> &'static str {
        match self {
            InferenceEndpoint::ChatCompletions => "/v1/chat/completions",
            InferenceEndpoint::Responses => "/v1/responses",
            InferenceEndpoint::Messages => "/v1/messages",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InferenceRequestKind {
    Foreground,
    Auxiliary,
}
impl InferenceRequestKind {
    fn classify(turn_index: Option<&str>, request_id: Option<&str>, body: &Value) -> Self {
        if turn_index.is_some() {
            return InferenceRequestKind::Foreground;
        }
        if request_id.is_some() {
            return InferenceRequestKind::Auxiliary;
        }
        if body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools.len() >= 2)
        {
            InferenceRequestKind::Foreground
        } else {
            InferenceRequestKind::Auxiliary
        }
    }
}
/// Tells a request posted again from a new one without keeping the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BodyHash(u64);
impl BodyHash {
    pub(crate) fn of(body: &Value) -> Self {
        let mut hasher = DefaultHasher::new();
        serde_json::to_string(body)
            .expect("serialize inference request body")
            .hash(&mut hasher);
        BodyHash(hasher.finish())
    }
}
/// Only a request with a non empty request id has one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RepostIdentity {
    endpoint: InferenceEndpoint,
    kind: InferenceRequestKind,
    request_id: String,
    body_hash: BodyHash,
}
/// A foreground request is exactly one with a conversation.
pub(crate) struct InferenceRequest<'a> {
    endpoint: InferenceEndpoint,
    kind: InferenceRequestKind,
    conversation: Option<ConversationId>,
    body_hash: BodyHash,
    repost_identity: Option<RepostIdentity>,
    body: &'a Value,
    headers: &'a HeaderMap,
}
impl<'a> InferenceRequest<'a> {
    /// Auxiliary requests take no number, so side queries never shift the number a script names.
    #[must_use = "a foreground request takes its conversation number here; log and answer it through the result"]
    pub(crate) fn new(
        conversations: &ConversationTracker,
        endpoint: InferenceEndpoint,
        headers: &'a HeaderMap,
        body: &'a Value,
    ) -> Self {
        let turn_index = nonempty_header(headers, TURN_INDEX_HEADER);
        let request_id = nonempty_header(headers, REQUEST_ID_HEADER);
        let session_id = nonempty_header(headers, SESSION_ID_HEADER);
        let kind = InferenceRequestKind::classify(turn_index, request_id, body);
        let body_hash = BodyHash::of(body);
        let repost_identity = request_id.map(|request_id| RepostIdentity {
            endpoint,
            kind,
            request_id: request_id.to_owned(),
            body_hash,
        });
        let conversation = match kind {
            InferenceRequestKind::Foreground => {
                Some(conversations.assign(ConversationKey::from_request(session_id, body)))
            }
            InferenceRequestKind::Auxiliary => None,
        };
        InferenceRequest {
            endpoint,
            kind,
            conversation,
            body_hash,
            repost_identity,
            body,
            headers,
        }
    }
    pub(crate) fn endpoint(&self) -> InferenceEndpoint {
        self.endpoint
    }
    pub(crate) fn kind(&self) -> InferenceRequestKind {
        self.kind
    }
    pub(crate) fn conversation(&self) -> Option<ConversationId> {
        self.conversation
    }
    pub(crate) fn body_hash(&self) -> BodyHash {
        self.body_hash
    }
    pub(crate) fn repost_identity(&self) -> Option<&RepostIdentity> {
        self.repost_identity.as_ref()
    }
    pub(crate) fn body(&self) -> &'a Value {
        self.body
    }
    pub(crate) fn headers(&self) -> &'a HeaderMap {
        self.headers
    }
}
fn nonempty_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
/// The default `/v1/models` entry; also the model a reply names when the request names none.
/// The one model the mock advertises unless a test names others.
pub const DEFAULT_MODEL: &str = "test-model";
pub(crate) fn model_name(body: &Value) -> &str {
    body.get("model")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_MODEL)
}
pub(crate) fn first_system_message(body: &Value) -> Option<String> {
    if let Some(system) = body.get("system") {
        return content_text(system);
    }
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        return Some(instructions.to_owned());
    }
    let first = items(body).next()?;
    let role = first.get("role").and_then(Value::as_str)?;
    if !matches!(role, "system" | "developer") {
        return None;
    }
    content_text(first.get("content")?)
}
pub(crate) fn last_user_message(body: &Value) -> Option<String> {
    last_message_text(body, "user")
}
pub(crate) fn last_assistant_message(body: &Value) -> Option<String> {
    last_message_text(body, "assistant")
}
fn last_message_text(body: &Value, role: &str) -> Option<String> {
    items(body)
        .rev()
        .find(|item| item.get("role").and_then(Value::as_str) == Some(role))
        .and_then(|item| content_text(item.get("content")?))
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryToolCall {
    pub(crate) name: String,
    pub(crate) arguments: Value,
}
pub(crate) fn history_tool_calls(body: &Value) -> Vec<HistoryToolCall> {
    let mut calls = Vec::new();
    let mut record = |name: Option<&Value>, arguments: Option<&Value>| {
        if let Some(name) = name.and_then(Value::as_str) {
            calls.push(HistoryToolCall {
                name: name.to_owned(),
                arguments: arguments.map(call_arguments).unwrap_or(Value::Null),
            });
        }
    };
    for item in items(body) {
        for call in item
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let function = call.get("function").unwrap_or(call);
            record(function.get("name"), function.get("arguments"));
        }
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            record(item.get("name"), item.get("arguments"));
        }
        for block in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                record(block.get("name"), block.get("input"));
            }
        }
    }
    calls
}
/// Chat Completions and Responses carry arguments as a JSON string; one that does not parse is
/// kept as that string so a comparison fails on it instead of the reader.
fn call_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| arguments.clone()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            arguments.clone()
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferedTool {
    pub(crate) name: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferedTools(Vec<OfferedTool>);
impl OfferedTools {
    pub(crate) fn has_tool(&self, name: &str) -> bool {
        self.find(name).is_some()
    }
    pub(crate) fn find(&self, name: &str) -> Option<&OfferedTool> {
        self.0.iter().find(|tool| tool.name == name)
    }
    pub(crate) fn names(&self) -> Vec<String> {
        self.0.iter().map(|tool| tool.name.clone()).collect()
    }
}
pub(crate) fn offered_tools(body: &Value) -> OfferedTools {
    OfferedTools(
        body.get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tool| {
                let function = tool.get("function").unwrap_or(tool);
                let name = function.get("name").and_then(Value::as_str)?;
                Some(OfferedTool {
                    name: name.to_owned(),
                })
            })
            .collect(),
    )
}
pub(crate) fn tool_results(body: &Value) -> BTreeMap<String, String> {
    let mut results = BTreeMap::new();
    let mut record = |call_id: Option<&Value>, content: Option<&Value>| {
        if let Some(call_id) = call_id.and_then(Value::as_str) {
            results.insert(
                call_id.to_owned(),
                content.and_then(content_text).unwrap_or_default(),
            );
        }
    };
    for item in items(body) {
        if item.get("role").and_then(Value::as_str) == Some("tool") {
            record(item.get("tool_call_id"), item.get("content"));
        }
        if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
            record(item.get("call_id"), item.get("output"));
        }
        for block in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                record(block.get("tool_use_id"), block.get("content"));
            }
        }
    }
    results
}
fn items(body: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    body.get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}
/// A string, or the `text` of each part joined by newlines; `None` without text.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => None,
    }
}
#[cfg(test)]
#[path = "inference_request_tests.rs"]
mod tests;
