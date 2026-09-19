//! Line counts a backend sends with a read whose file text travels as content.

use serde::{Deserialize, Serialize};

/// The raw output of a read whose file text is in the content blocks
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadLineCounts {
    pub total_lines: Option<usize>,
    /// Absent when the read covered the whole file
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ReadLineRange>,
}

/// One-based inclusive line range
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadLineRange {
    pub start: usize,
    pub end: usize,
}
