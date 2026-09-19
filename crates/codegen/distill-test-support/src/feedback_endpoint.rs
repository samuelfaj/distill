//! Mock `POST /v1/feedback`: records every submission and answers in cli-chat-proxy's `FeedbackResponse` shape.
//!
//! Every POST is recorded before the verdict is chosen, so a scripted failure still leaves the body a test can inspect.
//! Bodies are kept as loose JSON: tests assert parsed values, never the shell's wire struct.
//! A body over the capture cap keeps its text and metadata; only its `images` array is dropped.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::request_log::authorization_header;

/// Feedback bodies carry base64 images; anything larger than this is not worth holding in a test process.
const FEEDBACK_BODY_CAPTURE_CAP: usize = 256 * 1024;

/// One `POST /v1/feedback` the mock saw, accepted or scripted to fail.
#[derive(Debug, Clone)]
pub struct FeedbackPost {
    /// Parsed JSON body (`{"unparsed_feedback_body": ..}` when it was not JSON); above the capture cap its
    /// `images` is `Value::Null` and everything else is intact.
    pub body: Value,
    /// Raw `Authorization` header, if the client sent one.
    pub authorization: Option<String>,
}

/// The scripted verdict tests can flip, plus every POST seen through it.
#[derive(Default)]
pub(crate) struct FeedbackEndpointState {
    fail: AtomicBool,
    posts: Mutex<Vec<FeedbackPost>>,
}

impl FeedbackEndpointState {
    pub(crate) fn set_failure(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    pub(crate) fn posts(&self) -> Vec<FeedbackPost> {
        self.posts.lock().unwrap().clone()
    }

    /// A non-JSON body is kept as `{"unparsed_feedback_body": ..}` so a wire-shape regression stays visible.
    pub(crate) fn handle(&self, headers: &HeaderMap, raw: &[u8]) -> Response {
        let mut body = serde_json::from_slice::<Value>(raw)
            .unwrap_or_else(|_| json!({ "unparsed_feedback_body": String::from_utf8_lossy(raw) }));
        if raw.len() > FEEDBACK_BODY_CAPTURE_CAP
            && let Some(images) = body.get_mut("images")
        {
            *images = Value::Null;
        }
        let authorization = authorization_header(headers);
        let post_index = {
            let mut posts = self.posts.lock().unwrap();
            posts.push(FeedbackPost {
                body,
                authorization,
            });
            posts.len()
        };
        if self.fail.load(Ordering::SeqCst) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":"scripted feedback failure"}"#,
            )
                .into_response();
        }
        // `createdAt` must parse as RFC 3339 or the shell reads a 200 as a decode failure.
        (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "feedbackId": format!("mock-feedback-{post_index}"),
                "createdAt": "2026-01-01T00:00:00Z",
            })
            .to_string(),
        )
            .into_response()
    }
}

#[allow(clippy::disallowed_methods)] // test clients hit localhost mocks
#[cfg(test)]
#[path = "feedback_endpoint_tests.rs"]
mod tests;
