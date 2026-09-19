//! Mock `POST /v1/events`: records every product-telemetry batch the shell posts and answers 200.
//!
//! The telemetry client POSTs `GROK_TELEMETRY_EVENTS_URL` verbatim, so a test points that env at
//! `{url()}/events`. Events are flattened out of each batch so a test asserts on one `event_name` at a time.

use std::sync::Mutex;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// Every event seen so far, flattened out of the client's `events` batches in arrival order.
#[derive(Default)]
pub(crate) struct TelemetryEventsState {
    events: Mutex<Vec<Value>>,
}

impl TelemetryEventsState {
    pub(crate) fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }

    /// An unparseable body is kept as `{"unparsed_events_body": ..}` so a wire-shape regression stays visible.
    pub(crate) fn handle(&self, body: &[u8]) -> Response {
        let batch = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|parsed| parsed.get("events")?.as_array().cloned())
            .unwrap_or_else(|| {
                vec![json!({ "unparsed_events_body": String::from_utf8_lossy(body) })]
            });
        self.events.lock().unwrap().extend(batch);
        StatusCode::OK.into_response()
    }
}

#[allow(clippy::disallowed_methods)] // test clients hit localhost mocks
#[cfg(test)]
#[path = "telemetry_events_tests.rs"]
mod tests;
