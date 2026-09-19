//! Mock OTLP/HTTP server for the external stream, recording every post in an [`OtelRecorder`]. An
//! undecodable body is kept as an [`OtelFault`] and still answered 200, so the exporter does not
//! retry it.

use std::collections::HashMap;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;

use crate::loopback::{LoopbackServer, header, header_text, refuse_unserved};
use crate::otel_decode;
use crate::otel_event::{OtelFault, OtelSignal, OtelUnreadBody};
use crate::otel_recorder::{OtelRecorder, ReceivedBody};

/// Only has to exceed anything the exporter sends; a refused batch is recorded as a fault.
const MAX_ACCEPTED_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Short so a case sees its events without a long wait.
const EXPORT_INTERVAL_MS: u64 = 250;

#[must_use = "dropping stops the server"]
pub struct MockOtelServer {
    server: LoopbackServer,
    recorder: OtelRecorder,
}

impl MockOtelServer {
    /// Relative to [`MockOtelServer::origin`].
    pub fn path(signal: OtelSignal) -> &'static str {
        match signal {
            OtelSignal::Logs => "/v1/logs",
            OtelSignal::Metrics => "/v1/metrics",
        }
    }

    pub async fn start() -> anyhow::Result<MockOtelServer> {
        MockOtelServer::start_with_body_limit(MAX_ACCEPTED_BODY_BYTES).await
    }

    async fn start_with_body_limit(max_body_bytes: usize) -> anyhow::Result<MockOtelServer> {
        let recorder = OtelRecorder::new();
        let routes = OtelSignal::ALL
            .into_iter()
            .fold(Router::new(), |router, signal| {
                router.route(
                    MockOtelServer::path(signal),
                    post(
                        move |recorder: State<OtelRecorder>,
                              headers: HeaderMap,
                              payload: Result<Bytes, BytesRejection>| {
                            receive(recorder, signal, headers, payload)
                        },
                    ),
                )
            });
        let app = refuse_unserved(routes, |recorder: &OtelRecorder, request| {
            recorder.record_fault(OtelFault::Unserved {
                method: request.method.to_string(),
                path: request.uri.path().to_owned(),
            });
        })
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(recorder.clone());
        let server = LoopbackServer::serve(app, "mock OTLP server").await?;
        Ok(MockOtelServer { server, recorder })
    }

    pub fn recorder(&self) -> &OtelRecorder {
        &self.recorder
    }

    /// `http://127.0.0.1:<port>`: the OTLP base the exporter appends `/v1/<signal>` to.
    pub fn origin(&self) -> String {
        format!("http://{}", self.server.addr())
    }

    /// What `ExternalOtelConfig::resolve_with` needs to aim the external stream at this server.
    pub fn exporter_env(&self) -> HashMap<&'static str, String> {
        HashMap::from([
            ("GROK_EXTERNAL_OTEL", "1".to_owned()),
            ("OTEL_LOGS_EXPORTER", "otlp".to_owned()),
            ("OTEL_METRICS_EXPORTER", "otlp".to_owned()),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf".to_owned()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", self.origin()),
            ("OTEL_BLRP_SCHEDULE_DELAY", EXPORT_INTERVAL_MS.to_string()),
            (
                "OTEL_METRIC_EXPORT_INTERVAL",
                EXPORT_INTERVAL_MS.to_string(),
            ),
        ])
    }
}

async fn receive(
    State(recorder): State<OtelRecorder>,
    signal: OtelSignal,
    headers: HeaderMap,
    payload: Result<Bytes, BytesRejection>,
) -> StatusCode {
    let (status, received) = match &payload {
        Ok(bytes) => {
            let content_type = header(&headers, CONTENT_TYPE).unwrap_or_default();
            let received = ReceivedBody::InFull {
                bytes,
                decoded: otel_decode::decode_post(signal, &content_type, bytes),
            };
            (StatusCode::OK, received)
        }
        Err(rejection) => {
            let declared_len =
                header(&headers, CONTENT_LENGTH).and_then(|value| value.parse().ok());
            // Both rejection enums are `#[non_exhaustive]`.
            let body = match rejection {
                BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => {
                    OtelUnreadBody::Refused { declared_len }
                }
                _ => OtelUnreadBody::Incomplete { declared_len },
            };
            let received = ReceivedBody::Unread {
                body,
                error: rejection.body_text(),
            };
            (rejection.status(), received)
        }
    };
    let recorded_headers = headers
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), header_text(value)))
        .collect();
    recorder.record(signal, recorded_headers, received);
    status
}

#[cfg(test)]
#[path = "mock_otel_server_tests.rs"]
mod tests;
