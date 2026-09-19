// Modified for Distill by Samuel Fajreldines, 2026.
//! 401 attribution: callback hook + shared helpers for tool HTTP clients.

use std::sync::Arc;

use distill_auth::bearer_suffix;

pub use distill_auth::bearer_fragment::BEARER_SUFFIX_LEN;

/// Which tool endpoint produced the 401.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
pub enum ToolConsumer {
    #[strum(serialize = "ImageGen")]
    ImageGen,
    #[strum(serialize = "VideoGen.start")]
    VideoGenStart,
    #[strum(serialize = "VideoGen.poll")]
    VideoGenPoll,
    #[strum(serialize = "WebSearch")]
    WebSearch,
}
/// 401 attribution callback. Shell wires this to emit telemetry.
pub trait Auth401AttributionCallback: Send + Sync + std::fmt::Debug {
    /// `sent_bearer_suffix` is truncated to [`BEARER_SUFFIX_LEN`]
    /// before crossing this boundary. `None` = no bearer was sent.
    fn record_401(&self, consumer: ToolConsumer, sent_bearer_suffix: Option<&str>);
}

/// Shared, cheap-to-clone alias for the attribution callback.
pub type SharedAttributionCallback = Arc<dyn Auth401AttributionCallback>;

/// Record a 401 if a callback is wired, truncating to the tail first so only
/// the fragment is ever materialized.
pub(crate) fn emit_401(
    callback: Option<&SharedAttributionCallback>,
    consumer: ToolConsumer,
    sent_bearer: Option<&str>,
) {
    if let Some(cb) = callback {
        let suffix = sent_bearer.map(|s| bearer_suffix(s).to_string());
        cb.record_401(consumer, suffix.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_consumer_as_str_stable_identifiers() {
        assert_eq!(ToolConsumer::ImageGen.as_ref(), "ImageGen");
        assert_eq!(ToolConsumer::VideoGenStart.as_ref(), "VideoGen.start");
        assert_eq!(ToolConsumer::VideoGenPoll.as_ref(), "VideoGen.poll");
        assert_eq!(ToolConsumer::WebSearch.as_ref(), "WebSearch");
    }
}
