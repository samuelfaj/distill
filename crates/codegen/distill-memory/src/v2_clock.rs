//! Shared wall clock for memory-v2 state transitions.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Supplies non-negative Unix seconds to every memory-v2 component.
pub trait V2Clock: std::fmt::Debug + Send + Sync {
    fn now_unix_seconds(&self) -> i64;
}

pub type SharedV2Clock = Arc<dyn V2Clock>;

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemV2Clock;

impl V2Clock for SystemV2Clock {
    fn now_unix_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            })
    }
}

pub fn system_v2_clock() -> SharedV2Clock {
    Arc::new(SystemV2Clock)
}
