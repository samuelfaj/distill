// Modified for Distill by Samuel Fajreldines, 2026.
//! Emits `tracing` events with `target: "sampling_log"`.
//! A dedicated layer in `distill-telemetry` routes these to
//! `~/.grok/logs/sampling.jsonl`. Enable with `--log-sampling`.

use crate::types::RequestId;

pub const TARGET: &str = "sampling_log";

#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub auth_type: &'static str,
    pub auth_prefix: Option<String>,
}

pub fn request_span(
    request_id: &RequestId,
    model: &str,
    api_backend: &str,
    base_url: &str,
    auth: &AuthInfo,
) -> tracing::Span {
    tracing::info_span!(
        target: TARGET,
        "sampling.request",
        request_id = %request_id,
        model = model,
        api_backend = api_backend,
        base_url = base_url,
        auth_type = auth.auth_type,
        auth_prefix = auth.auth_prefix.as_deref().unwrap_or(""),
        // Recorded from `SamplerConfig` / response usage as the request progresses; `field::Empty` lets callers `record()` them later
        reasoning_effort = tracing::field::Empty,
        output_tokens = tracing::field::Empty,
        reasoning_tokens = tracing::field::Empty,
        total_attempts = tracing::field::Empty,
    )
}
