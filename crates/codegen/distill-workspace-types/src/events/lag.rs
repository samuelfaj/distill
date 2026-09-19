//! `EventLag` is a wire-format payload: it appears in the `EventEnvelope.payload.lag` oneof over the gRPC `Events` stream.
//! The canonical Rust definition therefore belongs in this crate.
//! The runtime `EventStream<T>` wrapper (in the workspace crate) will report lag to consumers as `Result<T, EventLag>`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Backpressure signal when the event-bus subscriber lags and events are dropped. Tagged with `tag = "type"` like every wire enum.
/// `Lagged(u64)` is the count dropped since the previous successful receive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventLag {
    /// The consumer fell behind by `n` events.
    #[error("lagged by {0} events")]
    Lagged(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_uses_type_tag_with_data_payload() {
        let lag = EventLag::Lagged(3);
        let json = serde_json::to_string(&lag).unwrap();
        assert_eq!(json, r#"{"type":"lagged","data":3}"#);
    }
}
