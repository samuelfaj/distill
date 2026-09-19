//! Data-driven scripted responses: status/header/body triples the mock inference server queues per path and the loopback mocks serve on request, rendered to HTTP at serve time.
//! Pure data: no router or handler types are public.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde_json::Value;

pub(crate) type BoxWait = Pin<Box<dyn Future<Output = ()> + Send>>;
pub(crate) type TerminalWait = Box<dyn FnOnce() -> BoxWait + Send>;

/// An SSE comment the hang body flushes so the response head reaches the client, then the stream
/// produces no chunk; a comment carries no event, so the client's idle timer runs from here.
const HANG_OPENING_FRAME: &[u8] = b": grok-mock stream open\n\n";

/// One SSE event as data: optional `event:` name plus the `data:` payload.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    pub fn with_event(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ScriptedBody {
    Json(Value),
    Sse(Vec<SseEvent>),
    /// Raw body bytes served verbatim, for byte-exact payloads such as malformed SSE.
    Raw(String),
    /// The connection closes before the response head reaches the client: the body stream fails on
    /// its first poll, so hyper tears the connection down without flushing the head it queued.
    Dropped,
    /// The head reaches the client, then the body stalls forever with no chunk, so the client's
    /// inference idle timeout fires.
    Hang,
}

/// A scripted reply; see `inference_override` for where it sits in the tier order.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ScriptedBody,
}

impl ScriptedResponse {
    /// 200 SSE response built from an event list.
    pub fn sse(events: Vec<SseEvent>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: ScriptedBody::Sse(events),
        }
    }

    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ScriptedBody::Json(body),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ScriptedBody::Raw(body.into()),
        }
    }

    pub fn dropped() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: ScriptedBody::Dropped,
        }
    }

    /// A 200 stream whose head reaches the client, then never yields a chunk, so the client's
    /// inference idle timeout fires.
    pub fn hang() -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            body: ScriptedBody::Hang,
        }
    }

    pub(crate) fn is_sse(&self) -> bool {
        matches!(self.body, ScriptedBody::Sse(_))
    }

    /// Validate status and headers eagerly so a bad script panics at the enqueue call site rather than far away at serve time.
    pub(crate) fn validate(&self) {
        StatusCode::from_u16(self.status).expect("invalid scripted status code");
        for (name, value) in &self.headers {
            HeaderName::from_bytes(name.as_bytes()).expect("invalid scripted header name");
            HeaderValue::from_str(value).expect("invalid scripted header value");
        }
    }

    /// Render to HTTP with SSE events paced by `delay` and `before_terminal` awaited before the last event.
    /// Non-SSE bodies await `before_terminal` before returning, so every body mode waits the same way.
    pub(crate) async fn into_response_paced(
        self,
        delay: Option<std::time::Duration>,
        before_terminal: Option<TerminalWait>,
    ) -> Response {
        let mut response = match self.body {
            ScriptedBody::Json(body_json) => {
                if let Some(wait) = before_terminal {
                    wait().await;
                }
                Json(body_json).into_response()
            }
            ScriptedBody::Raw(raw_body) => {
                if let Some(wait) = before_terminal {
                    wait().await;
                }
                raw_body.into_response()
            }
            ScriptedBody::Dropped => {
                if let Some(wait) = before_terminal {
                    wait().await;
                }
                Response::new(Body::from_stream(stream::once(async {
                    Err::<Bytes, _>(std::io::Error::other("the mock dropped the connection"))
                })))
            }
            ScriptedBody::Hang => {
                if let Some(wait) = before_terminal {
                    wait().await;
                }
                let body = stream::unfold(false, |flushed| async move {
                    if flushed {
                        std::future::pending::<()>().await;
                        None
                    } else {
                        Some((
                            Ok::<Bytes, std::io::Error>(Bytes::from_static(HANG_OPENING_FRAME)),
                            true,
                        ))
                    }
                });
                Response::new(Body::from_stream(body))
            }
            ScriptedBody::Sse(events) => {
                let last_index = events.len().checked_sub(1);
                let mut events: Vec<_> = events.into_iter().enumerate().map(Some).collect();
                if events.is_empty() && before_terminal.is_some() {
                    events.push(None);
                }
                let stream = stream::unfold(
                    (events.into_iter(), before_terminal),
                    move |(mut events, mut before_terminal)| async move {
                        loop {
                            let item = events.next()?;
                            let Some((index, scripted_event)) = item else {
                                if let Some(wait) = before_terminal.take() {
                                    wait().await;
                                }
                                continue;
                            };
                            if let Some(delay) = delay {
                                tokio::time::sleep(delay).await;
                            }
                            if Some(index) == last_index
                                && let Some(wait) = before_terminal.take()
                            {
                                wait().await;
                            }
                            let event =
                                axum::response::sse::Event::default().data(scripted_event.data);
                            let event = match scripted_event.event {
                                Some(name) => event.event(name),
                                None => event,
                            };
                            return Some((Ok::<_, Infallible>(event), (events, before_terminal)));
                        }
                    },
                );
                Sse::new(stream)
                    .keep_alive(KeepAlive::default())
                    .into_response()
            }
        };
        *response.status_mut() =
            StatusCode::from_u16(self.status).expect("valid scripted status code");
        for (name, value) in self.headers {
            response.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).expect("valid scripted header name"),
                HeaderValue::from_str(&value).expect("valid scripted header value"),
            );
        }
        response
    }
}
