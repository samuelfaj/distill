//! Error taxonomy for the Jev client (plan §12.5).
//!
//! Mapping: 400/401/403/404/422 and a structurally invalid 200 body are
//! [`JevErrorKind::Invalid`]; 408/429 are [`JevErrorKind::RateLimited`] (with
//! `retry-after` in ms); 5xx and 529 are [`JevErrorKind::Unavailable`] (529 is
//! *our* mapping — the vendor SDKs have no class for it); network/TLS failures
//! are [`JevErrorKind::Transport`]; running out of the caller-owned deadline is
//! [`JevErrorKind::Timeout`]. The hot path never retries: `is_retryable()` is
//! `false` for every kind by design, and the caller falls back to the existing
//! LLM path instead.

use std::fmt;

use super::types::Json;

/// Stable error categories; the shell maps these onto its own fallback path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevErrorKind {
    /// The deadline elapsed before a complete response (body included) arrived.
    Timeout,
    /// Connection/TLS failure or an interrupted response body.
    Transport,
    /// 408/429; carries `retry_after_ms` when the server sent `Retry-After`.
    RateLimited,
    /// 400/401/403/404/422 or a structurally invalid 200 body.
    Invalid,
    /// 5xx (including 529): the service is not usable right now.
    Unavailable,
}

impl JevErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::RateLimited => "rate_limited",
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for JevErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One Jev failure. `detail` is always safe to log: callers must build it from
/// status/shape information only, and [`JevError::from_status`] additionally
/// strips any credential that a server echoed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JevError {
    kind: JevErrorKind,
    status: Option<u16>,
    retry_after_ms: Option<u64>,
    field_path: Option<String>,
    request_id: Option<String>,
    detail: String,
}

impl JevError {
    pub fn new(kind: JevErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            retry_after_ms: None,
            field_path: None,
            request_id: None,
            detail: detail.into(),
        }
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(JevErrorKind::Invalid, detail)
    }

    pub fn transport(detail: impl Into<String>) -> Self {
        Self::new(JevErrorKind::Transport, detail)
    }

    pub fn timeout(detail: impl Into<String>) -> Self {
        Self::new(JevErrorKind::Timeout, detail)
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(JevErrorKind::Unavailable, detail)
    }

    /// Maps one non-2xx response onto the taxonomy, pulling `field_path` out of
    /// a validation error body when the server provides one.
    pub fn from_status(
        status: u16,
        body: Option<&Json>,
        request_id: Option<String>,
        retry_after_ms: Option<u64>,
    ) -> Self {
        let kind = match status {
            408 | 429 => JevErrorKind::RateLimited,
            400 | 401 | 403 | 404 | 422 => JevErrorKind::Invalid,
            500..=599 => JevErrorKind::Unavailable,
            _ => JevErrorKind::Invalid,
        };
        let field_path = body
            .and_then(|b| {
                b.get("error")
                    .and_then(|e| e.get("field_path").or_else(|| e.get("field")))
                    .or_else(|| b.get("field_path"))
            })
            .and_then(Json::as_str)
            .map(str::to_owned);
        Self {
            kind,
            status: Some(status),
            retry_after_ms,
            field_path,
            request_id,
            detail: format!("jev request failed with status {status}"),
        }
    }

    /// Replaces `secret` with `[redacted]` anywhere it appears in the message.
    /// Used defensively so a leaky server cannot put the API key into our logs.
    pub fn redact(mut self, secret: &str) -> Self {
        if !secret.is_empty() && self.detail.contains(secret) {
            self.detail = self.detail.replace(secret, "[redacted]");
        }
        self
    }

    pub const fn kind(&self) -> JevErrorKind {
        self.kind
    }

    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    pub const fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }

    pub fn field_path(&self) -> Option<&str> {
        self.field_path.as_deref()
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Always `false`: the permission hot path makes a single attempt and then
    /// falls back (plan §1.6/I-5). The SDK default of `maxRetries=2` is a
    /// deliberate divergence recorded in §12.5.
    pub const fn is_retryable(&self) -> bool {
        false
    }
}

impl fmt::Display for JevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "jev error ({}): {}", self.kind, self.detail)?;
        if let Some(status) = self.status {
            write!(f, " [status {status}]")?;
        }
        if let Some(ms) = self.retry_after_ms {
            write!(f, " [retry-after {ms}ms]")?;
        }
        if let Some(path) = &self.field_path {
            write!(f, " [field {path}]")?;
        }
        if let Some(req) = &self.request_id {
            write!(f, " [request {req}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for JevError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_matches_the_contract() {
        for status in [400u16, 401, 403, 404, 422] {
            assert_eq!(
                JevError::from_status(status, None, None, None).kind(),
                JevErrorKind::Invalid,
                "status {status} must map to Invalid"
            );
        }
        assert_eq!(
            JevError::from_status(408, None, None, Some(1500)).kind(),
            JevErrorKind::RateLimited
        );
        let limited = JevError::from_status(429, None, None, Some(2000));
        assert_eq!(limited.kind(), JevErrorKind::RateLimited);
        assert_eq!(limited.retry_after_ms(), Some(2000));
        for status in [500u16, 503, 529] {
            assert_eq!(
                JevError::from_status(status, None, None, None).kind(),
                JevErrorKind::Unavailable,
                "status {status} must map to Unavailable"
            );
        }
    }

    #[test]
    fn validation_body_contributes_a_field_path() {
        let body = serde_json::json!({"error": {"field_path": "answers.tone.confidence"}});
        let err = JevError::from_status(422, Some(&body), Some("req-7".into()), None);
        assert_eq!(err.field_path(), Some("answers.tone.confidence"));
        assert_eq!(err.request_id(), Some("req-7"));
        assert!(!err.is_retryable());
    }

    #[test]
    fn redaction_removes_an_echoed_credential() {
        let secret = "apikey_super_secret_value";
        let err = JevError::new(
            JevErrorKind::Invalid,
            format!("server said: token {secret} is not valid"),
        )
        .redact(secret);
        let shown = format!("{err}");
        assert!(!shown.contains(secret), "credential leaked into {shown}");
        assert!(shown.contains("[redacted]"));
    }

    #[test]
    fn display_carries_kind_status_and_retry_after() {
        let err = JevError::from_status(429, None, None, Some(900));
        let shown = format!("{err}");
        assert!(shown.contains("rate_limited"));
        assert!(shown.contains("retry-after 900ms"));
    }
}
