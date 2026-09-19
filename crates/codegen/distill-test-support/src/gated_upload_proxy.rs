//! A stand in for the trace upload endpoint that holds every request until opened, then forwards
//! it to the upstream mock over a raw one shot HTTP/1.1 roundtrip, so "uploads still pending" is
//! an ordering fact rather than a timing one.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::loopback::LoopbackServer;

#[must_use = "dropping stops the proxy"]
pub struct GatedUploadProxy {
    open_tx: tokio::sync::watch::Sender<bool>,
    url: String,
    _server: LoopbackServer,
}

struct GateState {
    upstream_authority: String,
    open_rx: tokio::sync::watch::Receiver<bool>,
}

async fn roundtrip_upstream(authority: &str, request: &[u8]) -> (StatusCode, Vec<u8>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut conn = tokio::net::TcpStream::connect(authority)
        .await
        .expect("connect to mock server");
    conn.write_all(request).await.expect("forward request");
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw)
        .await
        .expect("read mock response");

    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("mock response has headers");
    let head = String::from_utf8_lossy(raw.get(..header_end).unwrap_or(&[]));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .expect("mock response has a status");
    (status, raw.get(header_end + 4..).unwrap_or(&[]).to_vec())
}

async fn hold_then_forward(
    axum::extract::State(state): axum::extract::State<Arc<GateState>>,
    req: axum::extract::Request,
) -> Response {
    let mut open_rx = state.open_rx.clone();
    while !*open_rx.borrow() {
        // The proxy was dropped with this request still held.
        if open_rx.changed().await.is_err() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let path = req.uri().path().to_owned();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();

    let mut request = format!("{method} {path}{query} HTTP/1.1\r\n").into_bytes();
    request.extend_from_slice(format!("host: {}\r\n", state.upstream_authority).as_bytes());
    request.extend_from_slice(b"connection: close\r\n");
    request.extend_from_slice(format!("content-length: {}\r\n", body.len()).as_bytes());
    for (name, value) in headers.iter() {
        if ["host", "content-length", "connection", "transfer-encoding"].contains(&name.as_str()) {
            continue;
        }
        request.extend_from_slice(name.as_str().as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(&body);

    let (status, body) = roundtrip_upstream(&state.upstream_authority, &request).await;
    Response::builder()
        .status(status)
        .body(axum::body::Body::from(body))
        .expect("proxy response")
}

impl GatedUploadProxy {
    pub async fn start(upstream: String) -> anyhow::Result<GatedUploadProxy> {
        debug_assert!(
            upstream.starts_with("http://") && upstream.ends_with("/v1"),
            "GatedUploadProxy expects a MockInferenceServer http://<addr>/v1 URL, got {upstream:?}"
        );
        let (open_tx, open_rx) = tokio::sync::watch::channel(false);
        let upstream_authority = upstream
            .trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_owned();
        let state = Arc::new(GateState {
            upstream_authority,
            open_rx,
        });
        let app = Router::new().fallback(hold_then_forward).with_state(state);
        let server = LoopbackServer::serve(app, "gated upload proxy").await?;
        let url = format!("http://{}/v1", server.addr());
        Ok(GatedUploadProxy {
            open_tx,
            url,
            _server: server,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn open(&self) {
        let _ = self.open_tx.send(true);
    }
}
