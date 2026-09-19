//! Captures token-level timing from streaming inference responses: TTFB, TTLB, and inter-token latency (ITL) statistics.

use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Returns (p50, p99, max, mean, sum) from a slice of sorted values.
/// Panics if `sorted` is empty.
pub fn compute_percentiles(sorted: &[u64]) -> (u64, u64, u64, u64, u64) {
    let len = sorted.len();
    assert!(len > 0, "Cannot compute percentiles from empty slice");

    let Some(&p50) = sorted.get(len / 2) else {
        return (0, 0, 0, 0, 0);
    };
    let p99_idx = ((len as f64 * 0.99).ceil() as usize)
        .saturating_sub(1)
        .min(len - 1);
    let Some(&p99) = sorted.get(p99_idx) else {
        return (0, 0, 0, 0, 0);
    };
    let Some(&max) = sorted.last() else {
        return (0, 0, 0, 0, 0);
    };
    let sum: u64 = sorted.iter().sum();
    let mean = sum / len as u64;

    (p50, p99, max, mean, sum)
}

/// Per-response inference latency metrics computed from chunk timestamps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InferenceLatencyStats {
    /// Time to first content token (ms)
    pub time_to_first_token_ms: Option<u64>,
    /// Measured at stream exhaustion, not at the last content chunk, so it includes trailing metadata chunks.
    pub time_to_last_byte_ms: u64,
    /// Number of content chunks received
    pub chunk_count: u32,
    /// Inter-token latency intervals (raw data for session aggregation)
    pub itl_intervals_ms: Vec<u64>,
    pub itl_p50_ms: Option<u64>,
    pub itl_p99_ms: Option<u64>,
    pub itl_max_ms: Option<u64>,
    pub itl_mean_ms: Option<u64>,
    /// Total request attempts (`1` means no retries); set by the retry loop on success.
    pub attempts: u32,
}

impl InferenceLatencyStats {
    pub fn record_on_span(&self, span: &tracing::Span) {
        if let Some(ttfb) = self.time_to_first_token_ms {
            span.record("ttfb_ms", ttfb);
        }
        span.record("ttlb_ms", self.time_to_last_byte_ms);
        span.record("chunk_count", self.chunk_count);
        if let Some(p50) = self.itl_p50_ms {
            span.record("itl_p50_ms", p50);
        }
        if let Some(p99) = self.itl_p99_ms {
            span.record("itl_p99_ms", p99);
        }
    }

    /// `stream_start` - `Instant::now()` captured before initiating the stream.
    /// `chunk_timestamps` - `Instant` recorded on each content-bearing chunk.
    /// `stream_end` - `Instant::now()` captured after the stream is fully exhausted (after trailing metadata/`[DONE]` chunks). Used for TTLB.
    pub fn from_timestamps(
        stream_start: Instant,
        chunk_timestamps: &[Instant],
        stream_end: Instant,
    ) -> Self {
        let ttlb = stream_end.duration_since(stream_start).as_millis() as u64;

        if chunk_timestamps.is_empty() {
            return Self {
                time_to_last_byte_ms: ttlb,
                ..Default::default()
            };
        }

        let Some(first) = chunk_timestamps.first() else {
            return Self {
                time_to_last_byte_ms: ttlb,
                ..Default::default()
            };
        };
        let ttfb = first.duration_since(stream_start);

        let intervals: Vec<u64> = chunk_timestamps
            .windows(2)
            .map(|w| match w {
                [a, b] => b.duration_since(*a).as_millis() as u64,
                _ => 0,
            })
            .collect();

        let (itl_p50, itl_p99, itl_max, itl_mean) = if intervals.is_empty() {
            (None, None, None, None)
        } else {
            let mut sorted = intervals.clone();
            sorted.sort_unstable();
            let (p50, p99, max, mean, _sum) = compute_percentiles(&sorted);
            (Some(p50), Some(p99), Some(max), Some(mean))
        };

        Self {
            time_to_first_token_ms: Some(ttfb.as_millis() as u64),
            time_to_last_byte_ms: ttlb,
            chunk_count: u32::try_from(chunk_timestamps.len()).unwrap_or(u32::MAX),
            itl_intervals_ms: intervals,
            itl_p50_ms: itl_p50,
            itl_p99_ms: itl_p99,
            itl_max_ms: itl_max,
            itl_mean_ms: itl_mean,
            attempts: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn offset(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn test_empty_timestamps() {
        let start = Instant::now();
        let end = start + Duration::from_millis(500);

        let stats = InferenceLatencyStats::from_timestamps(start, &[], end);

        assert_eq!(stats.time_to_first_token_ms, None);
        assert_eq!(stats.time_to_last_byte_ms, 500);
        assert_eq!(stats.chunk_count, 0);
        assert_eq!(stats.itl_p50_ms, None);
        assert_eq!(stats.itl_p99_ms, None);
        assert_eq!(stats.itl_max_ms, None);
        assert_eq!(stats.itl_mean_ms, None);
    }

    #[test]
    fn test_single_chunk() {
        let start = Instant::now();
        let chunks = vec![offset(start, 100)];
        let end = offset(start, 200);

        let stats = InferenceLatencyStats::from_timestamps(start, &chunks, end);

        assert_eq!(stats.time_to_first_token_ms, Some(100));
        assert_eq!(stats.time_to_last_byte_ms, 200);
        assert_eq!(stats.chunk_count, 1);
        // One chunk yields no intervals, so there are no ITL stats
        assert_eq!(stats.itl_p50_ms, None);
        assert_eq!(stats.itl_p99_ms, None);
        assert_eq!(stats.itl_max_ms, None);
        assert_eq!(stats.itl_mean_ms, None);
    }

    #[test]
    fn test_two_chunks() {
        let start = Instant::now();
        let chunks = vec![offset(start, 100), offset(start, 150)];
        let end = offset(start, 200);

        let stats = InferenceLatencyStats::from_timestamps(start, &chunks, end);

        assert_eq!(stats.time_to_first_token_ms, Some(100));
        assert_eq!(stats.time_to_last_byte_ms, 200);
        assert_eq!(stats.chunk_count, 2);
        // One 50ms interval between the two chunks
        assert_eq!(stats.itl_p50_ms, Some(50));
        assert_eq!(stats.itl_p99_ms, Some(50));
        assert_eq!(stats.itl_max_ms, Some(50));
        assert_eq!(stats.itl_mean_ms, Some(50));
    }

    #[test]
    fn test_many_chunks() {
        let start = Instant::now();
        // 11 chunks: intervals are [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
        let chunks: Vec<Instant> = (0..11)
            .scan(100u64, |acc, i| {
                let t = *acc;
                *acc += (i + 1) * 10;
                Some(offset(start, t))
            })
            .collect();
        let end = offset(start, 1000);

        let stats = InferenceLatencyStats::from_timestamps(start, &chunks, end);

        assert_eq!(stats.time_to_first_token_ms, Some(100));
        assert_eq!(stats.time_to_last_byte_ms, 1000);
        assert_eq!(stats.chunk_count, 11);

        // p50 takes intervals[len / 2] = intervals[5]
        assert_eq!(stats.itl_p50_ms, Some(60));
        // p99_idx is ceil(10 * 0.99) - 1 = 9, so p99 reads intervals[9]
        assert_eq!(stats.itl_p99_ms, Some(100));
        assert_eq!(stats.itl_max_ms, Some(100));
        // The ten intervals sum to 550
        assert_eq!(stats.itl_mean_ms, Some(55));
    }

    #[test]
    fn test_p99_does_not_overflow() {
        let start = Instant::now();
        // 101 chunks give 100 intervals (indices 0..99)
        let chunks: Vec<Instant> = (0..101).map(|i| offset(start, 100 + i * 10)).collect();
        let end = offset(start, 2000);

        let stats = InferenceLatencyStats::from_timestamps(start, &chunks, end);

        assert_eq!(stats.chunk_count, 101);
        // All 100 intervals are 10ms
        // p99_idx is ceil(100 * 0.99) - 1 = 99, which stays in bounds
        assert_eq!(stats.itl_p99_ms, Some(10));
        assert_eq!(stats.itl_max_ms, Some(10));
        assert_eq!(stats.itl_p50_ms, Some(10));
        assert_eq!(stats.itl_mean_ms, Some(10));
    }

    #[test]
    fn test_ttlb_uses_stream_end_not_last_chunk() {
        let start = Instant::now();
        let chunks = vec![offset(start, 100), offset(start, 200)];
        // stream_end is 500ms after start, well past the last chunk at 200ms
        let end = offset(start, 500);

        let stats = InferenceLatencyStats::from_timestamps(start, &chunks, end);

        assert_eq!(stats.time_to_last_byte_ms, 500);
        assert_eq!(stats.time_to_first_token_ms, Some(100));
    }
}
