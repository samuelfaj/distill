//! External-OTEL configuration product telemetry events.

use serde::Serialize;

/// Emitted once per process (post-auth) when the external OTEL stream is configured.
/// Endpoint reduced to `scheme://host[:port]`; we measure adoption without learning collector details.
#[derive(Serialize)]
pub struct ExternalOtelConfigured {
    pub metrics_exporter: String,
    pub logs_exporter: String,
    pub protocol: String,
    pub logs_endpoint_origin: String,
    pub metrics_endpoint_origin: String,
    pub prompts_gate: bool,
    pub details_gate: bool,
    pub assistant_gate: bool,
    pub content_gate: bool,
    /// Startup source of the master switch: `env` | `config`.
    pub source: String,
}

/// Remote (fleet) policy applied to the external stream mid-run.
#[derive(Serialize)]
pub struct ExternalOtelRemotePolicyApplied {
    /// `force_disable` | `gates_locked`.
    pub action: String,
}

/// Export-health counters for the external stream, emitted on the internal pipeline at shutdown (never externally, to avoid feedback loops).
#[derive(Serialize)]
pub struct ExternalOtelExportHealth {
    pub records_dropped: u64,
    pub metric_exports_dropped: u64,
    pub export_failures: u64,
    pub export_successes: u64,
}
