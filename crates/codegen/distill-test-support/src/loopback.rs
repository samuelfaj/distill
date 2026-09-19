//! What the loopback doubles share: a server that stops when its owner drops, fallbacks that log
//! a request no route serves, and lossy header readers.

use std::net::SocketAddr;

use anyhow::Context as _;
use axum::Router;
use axum::extract::State;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use tokio::net::TcpListener;
use tokio_util::task::AbortOnDropHandle;

/// Aborting the serve task closes the listener; connections already accepted end when their
/// peer closes.
#[must_use = "dropping stops the server"]
pub(crate) struct LoopbackServer {
    addr: SocketAddr,
    _task: AbortOnDropHandle<()>,
}

impl LoopbackServer {
    pub(crate) async fn serve(app: Router, name: &'static str) -> anyhow::Result<LoopbackServer> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .with_context(|| format!("bind {name}"))?;
        let addr = listener.local_addr().context("local_addr")?;
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, name, "loopback server stopped serving");
            }
        }));
        Ok(LoopbackServer { addr, _task: task })
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// Logs a request no route serves before refusing it, so a misrouted client shows in the
/// double's log instead of as a timeout.
pub(crate) fn refuse_unserved<S>(
    router: Router<S>,
    record: impl Fn(&S, &Parts) + Clone + Send + Sync + 'static,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let record_not_found = record.clone();
    router
        .fallback(move |State(state): State<S>, request: Parts| async move {
            record_not_found(&state, &request);
            StatusCode::NOT_FOUND
        })
        .method_not_allowed_fallback(move |State(state): State<S>, request: Parts| async move {
            record(&state, &request);
            StatusCode::METHOD_NOT_ALLOWED
        })
}

/// A header value as text; one that is not UTF-8 is recorded lossily rather than dropped.
pub(crate) fn header_text(value: &HeaderValue) -> String {
    String::from_utf8_lossy(value.as_bytes()).into_owned()
}

pub(crate) fn header(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers.get(name).map(header_text)
}
