//! Declarative table of external OTEL metrics.
//!
//! One [`metrics!`] row is the single definition of a metric: it generates the
//! [`MetricIncrement`] variant, the wire-name const, the [`Instruments`] field and
//! its builder, and the increment-dispatch arm. Adding a metric is one row; a test
//! pins the names, units, and attribute keys the rows produce.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

/// Default OTel buckets end at 10s; startup failures and slow first tokens land in the 10-120s range, so those samples need real buckets, not +Inf.
const LATENCY_MS_BOUNDARIES: &[f64] = &[
    50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 15000.0, 30000.0, 60000.0, 120000.0,
];

fn ms_histogram(meter: &Meter, name: &'static str) -> Histogram<u64> {
    meter
        .u64_histogram(name)
        .with_unit("ms")
        .with_boundaries(LATENCY_MS_BOUNDARIES.to_vec())
        .build()
}

/// `model` is the one non-enum metric attribute value: scrub it at increment time rather than trusting every call site.
/// A collector fixture pins this by asserting on the wire payload.
fn scrub(s: &str) -> String {
    crate::redact_common::redact_to_owned(s)
}

/// Generate the metric increment enum, wire-name consts, instrument set, and dispatch from one row per metric.
///
/// Per row: the variant name and payload fields, the instrument (`counter_u64` /
/// `counter_f64` with a unit, or `histogram` on the shared `ms` buckets), the wire
/// name, the per-metric attributes (`plain` verbatim or `scrub`bed), and the op that
/// records it (`add` for counters, `record` for histograms).
macro_rules! metrics {
    ( $(
        $(#[$vmeta:meta])*
        $variant:ident {
            wire: $wire:literal,
            const: $const:ident,
            field: $field:ident : $kind:tt $(($unit:literal))?,
            $( payload: { $( $pf:ident : $pty:ty ),+ $(,)? }, )?
            attrs: [ $( $akey:literal => $amode:tt($afield:ident) ),* $(,)? ],
            op: $op:ident($amount:tt),
        }
    )* ) => {
        #[derive(Debug, Clone, PartialEq)]
        pub enum MetricIncrement {
            $(
                $(#[$vmeta])*
                $variant $( { $( $pf: $pty ),+ } )?,
            )*
        }

        $( pub(crate) const $const: &str = $wire; )*

        /// Pre-created instruments; a test pins their names/units/attrs.
        pub(crate) struct Instruments {
            $( $field: metrics!(@ty $kind), )*
        }

        impl Instruments {
            pub(crate) fn new(meter: &Meter) -> Self {
                Self {
                    $( $field: metrics!(@build meter, $kind, $const $(, $unit)?), )*
                }
            }

            pub(crate) fn record_increment(&self, increment: MetricIncrement, mut attrs: Vec<KeyValue>) {
                match increment {
                    $(
                        MetricIncrement::$variant $( { $( $pf ),+ } )? => {
                            $( attrs.push(KeyValue::new($akey, metrics!(@val $amode $afield))); )*
                            self.$field.$op($amount, &attrs);
                        }
                    )*
                }
            }
        }
    };

    (@ty counter_u64) => { Counter<u64> };
    (@ty counter_f64) => { Counter<f64> };
    (@ty histogram) => { Histogram<u64> };

    (@build $meter:ident, counter_u64, $const:ident, $unit:literal) => {
        $meter.u64_counter($const).with_unit($unit).build()
    };
    (@build $meter:ident, counter_f64, $const:ident, $unit:literal) => {
        $meter.f64_counter($const).with_unit($unit).build()
    };
    (@build $meter:ident, histogram, $const:ident) => {
        ms_histogram($meter, $const)
    };

    (@val plain $f:ident) => { $f };
    (@val scrub $f:ident) => { scrub(&$f) };
}

metrics! {
    SessionCount {
        wire: "grok_code.session.count",
        const: METRIC_SESSION_COUNT,
        field: session_count: counter_u64("{session}"),
        attrs: [],
        op: add(1),
    }
    TokenUsage {
        wire: "grok_code.token.usage",
        const: METRIC_TOKEN_USAGE,
        field: token_usage: counter_u64("{token}"),
        payload: { token_type: &'static str, model: String, count: u64 },
        attrs: [ "type" => plain(token_type), "model" => scrub(model) ],
        op: add(count),
    }
    CostUsage {
        wire: "grok_code.cost.usage",
        const: METRIC_COST_USAGE,
        field: cost_usage: counter_f64("USD"),
        payload: { model: String, cost_usd: f64 },
        attrs: [ "model" => scrub(model) ],
        op: add(cost_usd),
    }
    TurnCount {
        wire: "grok_code.turn.count",
        const: METRIC_TURN_COUNT,
        field: turn_count: counter_u64("{turn}"),
        payload: { outcome: &'static str, model: String },
        attrs: [ "outcome" => plain(outcome), "model" => scrub(model) ],
        op: add(1),
    }
    /// `grok_code.turn.ttft` (ms from turn start to the first token of any channel: reasoning, text, or a tool call).
    TurnTtft {
        wire: "grok_code.turn.ttft",
        const: METRIC_TURN_TTFT,
        field: turn_ttft: histogram,
        payload: { duration_ms: u64, model: String },
        attrs: [ "model" => scrub(model) ],
        op: record(duration_ms),
    }
    /// `grok_code.turn.ttfm` (ms from turn start to the first assistant text message; reasoning and tool calls are excluded).
    TurnTtfm {
        wire: "grok_code.turn.ttfm",
        const: METRIC_TURN_TTFM,
        field: turn_ttfm: histogram,
        payload: { duration_ms: u64, model: String },
        attrs: [ "model" => scrub(model) ],
        op: record(duration_ms),
    }
    ToolDecision {
        wire: "grok_code.tool.decision",
        const: METRIC_TOOL_DECISION,
        field: tool_decision: counter_u64("{decision}"),
        payload: { tool_name: String, decision: &'static str, access_kind: &'static str, permission_mode: &'static str },
        attrs: [
            "tool_name" => scrub(tool_name),
            "decision" => plain(decision),
            "access_kind" => plain(access_kind),
            "permission_mode" => plain(permission_mode),
        ],
        op: add(1),
    }
    ToolUsage {
        wire: "grok_code.tool.usage",
        const: METRIC_TOOL_USAGE,
        field: tool_usage: counter_u64("{call}"),
        payload: { tool_name: String, outcome: &'static str, model: String },
        attrs: [ "tool_name" => scrub(tool_name), "outcome" => plain(outcome), "model" => scrub(model) ],
        op: add(1),
    }
    ErrorCount {
        wire: "grok_code.error.count",
        const: METRIC_ERROR_COUNT,
        field: error_count: counter_u64("{error}"),
        payload: { error_category: String, model: String },
        attrs: [ "error_category" => scrub(error_category), "model" => scrub(model) ],
        op: add(1),
    }
    StartupTimeout {
        wire: "grok_code.startup.timeout",
        const: METRIC_STARTUP_TIMEOUT,
        field: startup_timeout: counter_u64("{timeout}"),
        payload: { stuck_in: String, auth_mode: String },
        attrs: [ "stuck_in" => scrub(stuck_in), "auth_mode" => plain(auth_mode) ],
        op: add(1),
    }
    SessionCreateTimeout {
        wire: "grok_code.session.create_timeout",
        const: METRIC_SESSION_CREATE_TIMEOUT,
        field: session_create_timeout: counter_u64("{timeout}"),
        payload: { stuck_in: String },
        attrs: [ "stuck_in" => scrub(stuck_in) ],
        op: add(1),
    }
    StartupPhaseDuration {
        wire: "grok_code.startup.phase_duration",
        const: METRIC_STARTUP_PHASE_DURATION,
        field: startup_phase_duration: histogram,
        payload: { phase: String, duration_ms: u64, outcome: String, auth_mode: String },
        attrs: [ "phase" => scrub(phase), "outcome" => plain(outcome), "auth_mode" => plain(auth_mode) ],
        op: record(duration_ms),
    }
    StartupTotal {
        wire: "grok_code.startup.total",
        const: METRIC_STARTUP_TOTAL,
        field: startup_total: histogram,
        payload: { duration_ms: u64, outcome: String, auth_mode: String },
        attrs: [ "outcome" => plain(outcome), "auth_mode" => plain(auth_mode) ],
        op: record(duration_ms),
    }
    StartupInteractive {
        wire: "grok_code.startup.interactive",
        const: METRIC_STARTUP_INTERACTIVE,
        field: startup_interactive: histogram,
        payload: { duration_ms: u64, auth_mode: String },
        attrs: [ "auth_mode" => plain(auth_mode) ],
        op: record(duration_ms),
    }
    StartupSubTimerDuration {
        wire: "grok_code.startup.subtimer_duration",
        const: METRIC_STARTUP_SUBTIMER_DURATION,
        field: startup_subtimer_duration: histogram,
        payload: { phase: String, duration_ms: u64, outcome: String, auth_mode: String },
        attrs: [ "phase" => scrub(phase), "outcome" => plain(outcome), "auth_mode" => plain(auth_mode) ],
        op: record(duration_ms),
    }
}
