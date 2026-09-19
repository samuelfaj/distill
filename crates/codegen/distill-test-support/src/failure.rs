//! The failures a script entry answers with in place of its content, and the record of what the
//! mock did to a request. A failure answers the entry's request without advancing the script, so the
//! client's retry meets the same position.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use crate::scripted::ScriptedResponse;

/// The text of a reply cut short at the output token limit.
pub const CUT_REPLY: &str = "MOCK-CUT-PART-ONE";
/// The text of a reply from a model caught looping.
pub const LOOPING_REPLY: &str = "MOCK-LOOP MOCK-LOOP MOCK-LOOP MOCK-LOOP";
/// The header a client sends to opt into the inference API's loop detector.
pub const DOOM_LOOP_CHECK_HEADER: &str = "x-grok-doom-loop-check";
/// A single SSE data frame whose payload is not valid JSON, so the client's stream decoder fails to
/// deserialize a chunk and surfaces a serialization error.
pub(crate) const MALFORMED_SSE_BODY: &str = "data: {grok-mock malformed chunk\n\n";
/// The detector report a looping reply carries: the tightest tail repetition on the thinking channel.
pub const DOOM_LOOP_TRIGGER: &str = "tail_repetition:2@thinking";

/// Refuse the next `count` requests with an HTTP status, the way an upstream at capacity or an
/// expired credential does; not a model declining to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct StatusFailure {
    pub(crate) status: u16,
    pub(crate) count: usize,
    pub(crate) retry_after: Option<Duration>,
    pub(crate) body: Option<String>,
}

impl StatusFailure {
    /// A status outside 100 to 999 panics here rather than in the handler.
    pub fn new(status: u16) -> Self {
        assert!(
            StatusCode::from_u16(status).is_ok(),
            "invalid failure status {status}"
        );
        StatusFailure {
            status,
            count: 1,
            retry_after: None,
            body: None,
        }
    }

    pub fn with_count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Sent as `Retry-After` in whole seconds.
    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub(crate) fn into_scripted_response(self) -> ScriptedResponse {
        let mut response = match self.body {
            Some(body) => ScriptedResponse::text(self.status, body),
            None => ScriptedResponse::json(
                self.status,
                json!({ "error": { "message": format!("mock upstream failure {}", self.status), "type": "server_error" } }),
            ),
        };
        if let Some(retry_after) = self.retry_after {
            response
                .headers
                .push(("retry-after".to_owned(), retry_after.as_secs().to_string()));
        }
        response
    }
}

/// Where the error event sits in the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPosition {
    /// The first and only frame.
    First,
    /// After the frame that opens a reply.
    Midway,
}

/// Accept the next `count` requests and fail their streams with one error event in the endpoint's format.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct StreamError {
    pub(crate) count: usize,
    pub(crate) message: String,
    pub(crate) position: ErrorPosition,
}

impl StreamError {
    pub const DEFAULT_MESSAGE: &'static str =
        "Service temporarily unavailable. The model did not respond to this request.";

    pub fn new() -> Self {
        StreamError {
            count: 1,
            message: StreamError::DEFAULT_MESSAGE.to_owned(),
            position: ErrorPosition::First,
        }
    }

    pub fn with_count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    pub fn with_position(mut self, position: ErrorPosition) -> Self {
        self.position = position;
        self
    }
}

impl Default for StreamError {
    fn default() -> Self {
        StreamError::new()
    }
}

/// One of an entry's failures, with how many of its requests it answers in place of the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Status(StatusFailure),
    StreamError(StreamError),
    /// The cut reply, stopped at the output token limit.
    Cut {
        count: usize,
    },
    /// The entry's first tool call again, or the looping reply.
    DoomLoop {
        count: usize,
    },
    /// The connection closed with no body.
    Dropped {
        count: usize,
    },
    /// A body the client's stream decoder cannot parse.
    MalformedBody {
        count: usize,
    },
    /// The stream opened then never sent a chunk, so the client's inference idle timeout fired.
    Hang {
        count: usize,
    },
}

impl Failure {
    pub(crate) fn count(&self) -> usize {
        match self {
            Failure::Status(failure) => failure.count,
            Failure::StreamError(stream_error) => stream_error.count,
            Failure::Cut { count }
            | Failure::DoomLoop { count }
            | Failure::Dropped { count }
            | Failure::MalformedBody { count }
            | Failure::Hang { count } => *count,
        }
    }

    pub(crate) fn observed(&self) -> ObservedFailure {
        match self {
            Failure::Status(failure) => ObservedFailure::Status(failure.status),
            Failure::StreamError(_) => ObservedFailure::StreamError,
            Failure::Cut { .. } => ObservedFailure::Cut,
            Failure::DoomLoop { .. } => ObservedFailure::DoomLoop,
            Failure::Dropped { .. } => ObservedFailure::Dropped,
            Failure::MalformedBody { .. } => ObservedFailure::Malformed,
            Failure::Hang { .. } => ObservedFailure::Hung,
        }
    }
}

/// What the mock did to one request in place of answering it plainly, on its log entry. A stall
/// combined with another failure records that failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedFailure {
    Status(u16),
    /// Held before the first byte, then answered.
    Stalled,
    /// The connection closed before the response head reached the client.
    Dropped,
    /// Answered short at the output token limit.
    Cut,
    /// Accepted, then failed with an error event inside the stream.
    StreamError,
    /// Answered with the entry's first tool call again, or with the looping reply.
    DoomLoop,
    /// Answered with a body the client's stream decoder cannot parse.
    Malformed,
    /// Opened the stream then never sent a chunk, so the client's idle timeout fired.
    Hung,
}
