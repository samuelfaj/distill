//! Mock `POST /v1/storage`: counts every upload attempt, rejects them all while the 401 gate is
//! closed, and records the accepted ones.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::request_log::authorization_header;

const STORAGE_BODY_CAPTURE_CAP: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct StorageUpload {
    pub path: String,
    pub size: usize,
    /// Empty when `size` exceeds `STORAGE_BODY_CAPTURE_CAP`.
    pub body: Vec<u8>,
    pub authorization: Option<String>,
}

#[derive(Default)]
pub(crate) struct StorageEndpointState {
    unauthorized: AtomicBool,
    request_count: AtomicU32,
    uploads: Mutex<Vec<StorageUpload>>,
}

impl StorageEndpointState {
    pub(crate) fn set_unauthorized(&self, unauthorized: bool) {
        self.unauthorized.store(unauthorized, Ordering::SeqCst);
    }

    /// Every attempt, the rejected ones included.
    pub(crate) fn request_count(&self) -> u32 {
        self.request_count.load(Ordering::SeqCst)
    }

    pub(crate) fn uploads(&self) -> Vec<StorageUpload> {
        self.uploads.lock().unwrap().clone()
    }

    /// An accepted upload is answered in the proxy's `UploadResponse` shape.
    pub(crate) fn handle(&self, headers: &HeaderMap, body: &[u8]) -> Response {
        self.request_count.fetch_add(1, Ordering::SeqCst);
        if self.unauthorized.load(Ordering::SeqCst) {
            return (
                StatusCode::UNAUTHORIZED,
                r#"{"error":"Invalid or expired credentials (mock)"}"#,
            )
                .into_response();
        }

        let path = headers
            .get("X-Storage-Path")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let size = body.len();
        let captured_body = if size <= STORAGE_BODY_CAPTURE_CAP {
            body.to_vec()
        } else {
            Vec::new()
        };
        let authorization = authorization_header(headers);
        let response = (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "bucket": "mock-bucket",
                "path": path,
                "size": size,
                "content_type": "application/octet-stream",
                "generation": 1,
            })
            .to_string(),
        );
        self.uploads.lock().unwrap().push(StorageUpload {
            path,
            size,
            body: captured_body,
            authorization,
        });
        response.into_response()
    }
}
