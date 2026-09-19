//! The one handler behind the three inference endpoints. A request is admitted, logged, and offered
//! to the overrides; one they decline is answered by the next queued agent turn or the response
//! mode, in the format of the endpoint it arrived on.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{MethodRouter, post};
use serde_json::Value;

use crate::conversation::{ConversationTracker, ReadConversation};
use crate::inference_override::InferenceOverrides;
use crate::inference_request::{
    InferenceEndpoint, InferenceRequest, InferenceRequestKind, last_user_message, model_name,
};
use crate::request_log::RequestLog;
use crate::scripted::ScriptedResponse;
use crate::sse;

#[derive(Clone)]
enum ResponseMode {
    /// `Echo: <last user message>`, with whitespace collapsed.
    Echo,
    /// Deltas reconstruct the text byte for byte, newlines included.
    Fixed(String),
}

impl ResponseMode {
    fn text(&self, body: &Value) -> Cow<'_, str> {
        match self {
            ResponseMode::Echo => {
                let user_message = last_user_message(body).unwrap_or_else(|| "hello".to_owned());
                Cow::Owned(format!("Echo: {user_message}"))
            }
            ResponseMode::Fixed(text) => Cow::Borrowed(text),
        }
    }
}

/// Read once per request so one request sees one setting of each.
#[derive(Clone)]
struct FallbackSettings {
    mode: ResponseMode,
    /// `stop_reason` on the `/v1/messages` terminal `message_delta`.
    messages_stop_reason: String,
    chunk_delay: Option<Duration>,
}

#[derive(Clone)]
pub(crate) struct InferenceRoute {
    log: Arc<RequestLog>,
    conversations: ConversationTracker,
    overrides: InferenceOverrides,
    fallback: Arc<std::sync::RwLock<FallbackSettings>>,
    /// One assistant text per foreground turn, consumed in order.
    agent_turns: Arc<std::sync::Mutex<VecDeque<String>>>,
}

impl InferenceRoute {
    pub(crate) fn new(log: Arc<RequestLog>, overrides: InferenceOverrides) -> Self {
        InferenceRoute {
            log,
            conversations: ConversationTracker::default(),
            overrides,
            fallback: Arc::new(std::sync::RwLock::new(FallbackSettings {
                mode: ResponseMode::Echo,
                messages_stop_reason: "end_turn".to_owned(),
                chunk_delay: None,
            })),
            agent_turns: Arc::new(std::sync::Mutex::new(VecDeque::new())),
        }
    }

    pub(crate) fn conversations(&self) -> Vec<ReadConversation> {
        self.conversations.snapshot(&self.log.entries())
    }

    pub(crate) fn set_response(&self, text: String) {
        self.fallback.write().unwrap().mode = ResponseMode::Fixed(text);
    }

    pub(crate) fn set_agent_turns(&self, turns: VecDeque<String>) {
        *self.agent_turns.lock().unwrap() = turns;
    }

    pub(crate) fn set_messages_stop_reason(&self, stop_reason: String) {
        self.fallback.write().unwrap().messages_stop_reason = stop_reason;
    }

    pub(crate) fn set_chunk_delay(&self, delay: Option<Duration>) {
        self.fallback.write().unwrap().chunk_delay = delay;
    }

    pub(crate) fn handler(&self, endpoint: InferenceEndpoint) -> MethodRouter {
        let route = self.clone();
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            route.clone().serve(endpoint, headers, body)
        })
    }

    async fn serve(self, endpoint: InferenceEndpoint, headers: HeaderMap, body: Value) -> Response {
        let request = InferenceRequest::new(&self.conversations, endpoint, &headers, &body);
        let sequence = self.log.record_inference(&request);
        let settings = self.fallback.read().unwrap().clone();
        if let Some(response) = self
            .overrides
            .response_override(&request, settings.chunk_delay, |failure| {
                self.log.note_failure(sequence, failure)
            })
            .await
        {
            return response;
        }

        let agent_turn = (request.kind() == InferenceRequestKind::Foreground)
            .then(|| self.agent_turns.lock().unwrap().pop_front())
            .flatten();
        let mode = match agent_turn {
            Some(text) => ResponseMode::Fixed(text),
            None => settings.mode,
        };
        let text = mode.text(&body);
        let model = model_name(&body);
        let events = match (endpoint, &mode) {
            (InferenceEndpoint::ChatCompletions, ResponseMode::Echo) => {
                sse::chat_completion_script(&text, model)
            }
            (InferenceEndpoint::ChatCompletions, ResponseMode::Fixed(_)) => {
                sse::chat_completion_script_exact(&text, model)
            }
            (InferenceEndpoint::Responses, ResponseMode::Echo) => {
                sse::responses_api_script(&text, model)
            }
            (InferenceEndpoint::Responses, ResponseMode::Fixed(_)) => {
                sse::responses_api_script_exact(&text, model)
            }
            (InferenceEndpoint::Messages, ResponseMode::Echo | ResponseMode::Fixed(_)) => {
                sse::messages_api_script(&text, model, &settings.messages_stop_reason)
            }
        };
        let wait = self.overrides.fallback_terminal_wait(&request);
        ScriptedResponse::sse(events)
            .into_response_paced(settings.chunk_delay, wait)
            .await
    }
}
