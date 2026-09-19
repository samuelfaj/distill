//! Memory-flush product telemetry events.

use serde::Serialize;

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRetrievalMode {
    Disabled,
    FtsOnly,
    Hybrid,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFlushTrigger {
    SlashCommand,
    Interval,
    PreCompaction,
    UserRequested,
}

#[derive(Serialize)]
pub struct MemoryFlushed {
    pub trigger: MemoryFlushTrigger,
    pub success: bool,
    pub duration_ms: u64,
    pub response_length: usize,
}
