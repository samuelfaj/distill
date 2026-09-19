//! Process resource product telemetry events.

use serde::Serialize;

/// Why a [`ProcessResourceUsage`] was sampled, so a mid-life reading is not read as a post-teardown one.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ResourceReportTrigger {
    SessionClose,
    Periodic,
}

/// The ceilings this process runs under.
/// The denominator for `ProcessResourceUsage`: usage against limits is headroom.
#[derive(Serialize)]
pub struct ProcessResourceLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nofile_soft: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nofile_hard: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nproc_soft: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nproc_hard: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_parallelism: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroup_pids_max: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroup_memory_max: Option<String>,
}

/// Emitted when the jemalloc heap monitor crosses a configured threshold.
/// The acute signal that a build is growing without bound.
#[derive(Serialize)]
pub struct HeapThresholdCrossed {
    pub threshold_bytes: u64,
    pub resident_bytes: u64,
    pub allocated_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_peak_bytes: Option<u64>,
}

/// What this process still holds just after a session was removed.
/// Aggregated per release, a rising tail is a leak.
/// `resident_sessions` separates leader mode, where one process serves many sessions and a leak compounds.
#[derive(Serialize)]
pub struct ProcessResourceUsage {
    pub trigger: ResourceReportTrigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_files: Option<u64>,
    pub resident_sessions: usize,
    pub session_threads: usize,
    pub idle: bool,
}
