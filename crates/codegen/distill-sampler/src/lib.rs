// Modified for Distill by Samuel Fajreldines, 2026.
//! Actor-based sampling layer for Grok grok.
//!
//! This crate holds the HTTP streaming and retry logic extracted from `distill-shell`'s session actor.
//! It is built on the same actor pattern as `distill-hunk-tracker`.
//!
//! ## Layered API
//!
//! - **Layer 1**: [`client::SamplingClient`] returns raw chunk streams.
//! - **Layer 2**: [`stream`] transforms raw streams into [`SamplingEvent`]s.
//! - **Layer 3**: [`SamplerHandle`] manages concurrent requests with retry, cancellation, and event-based coordination via the actor.

#![deny(clippy::indexing_slicing)]

pub mod actor;
pub mod attribution;
pub mod client;
pub mod commands;
pub mod config;
pub mod doom_loop;
mod doom_loop_recovery;
pub mod events;
pub mod handle;
pub mod metrics;
mod prewarm;
mod request_compression;
pub mod retry;
pub mod sampling_log;
mod shared_http;
mod span_timing;
pub mod stream;
mod stream_classify;
pub mod types;

// Public re-exports: the API consumers see
pub use actor::SamplerActor;
pub use actor::request_task::CompletionResult;
pub use attribution::{
    Auth401AttributionCallback, BEARER_SUFFIX_LEN, SamplingConsumer, SharedAttributionCallback,
};
pub use client::{ApiBackend, SamplingClient, user_agent_string_for};
pub use config::{
    AuthScheme, BearerResolver, HeaderInjector, OriginClientInfo, RequestCompression, RetryPolicy,
    SamplerConfig, SharedBearerResolver, SharedHeaderInjector,
};
pub use doom_loop::DoomLoopSignalCollector;
pub use events::{
    SamplingChannel, SamplingErrorInfo, SamplingErrorKind, SamplingEvent, StripReason,
};
pub use handle::{CollectedSamplingResult, DoomLoopRecoveryAttempt, SamplerHandle};
pub use metrics::{InferenceLatencyStats, compute_percentiles};
pub use prewarm::{PrewarmOutcome, PrewarmReport, prewarm_transport};
pub use retry::{
    DEFAULT_MAX_RETRIES, MAX_RETRY_BACKOFF, RATE_LIMIT_RETRY_DISABLED, RATE_LIMIT_RETRY_THRESHOLD,
    RetryDecision, classify_error, format_sampling_error, jitter_backoff, resolve_max_retries,
    retry_after_or_backoff, retry_backoff_with_jitter,
};
pub use sampling_log::AuthInfo;
pub use stream::{collect_response, stream_chat_completions, stream_messages, stream_responses};
pub use types::RequestId;
pub use distill_sampling_types::ConversationGroupId;
