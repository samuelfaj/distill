//! API/internal error product telemetry events.

use serde::Serialize;

/// Emitted when a user's turn fails due to rate limiting (all retries exhausted).
/// Key conversion-funnel signal: rate limit, then upsell, then subscribe.
#[derive(Serialize)]
pub struct RateLimitHit {
    pub model_id: String,
    /// Number of retry attempts before giving up.
    pub attempts: u32,
}

/// Model-API failure at the turn level (non-rate-limit).
/// Category/class only, no message text (external `api_error` event; also a product event).
#[derive(Serialize)]
pub struct ApiError {
    /// Fixed classification (`auth`, `server_error`, `timeout`, …).
    pub error_category: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Internal (our-code) error class for the external `internal_error` event.
/// Error class only: no message, no location.
#[derive(Serialize)]
pub struct InternalError {
    pub error_type: String,
}
