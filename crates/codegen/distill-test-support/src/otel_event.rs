//! What the mock OTLP server records: one exported OTLP log record or metric point in typed form, the
//! export that carried it, and the faults a post can leave instead.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

/// Attribute values flattened to JSON so a case compares by parsed value.
pub type OtelAttributes = BTreeMap<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtelSignal {
    Logs,
    Metrics,
}

impl OtelSignal {
    pub const ALL: [OtelSignal; 2] = [OtelSignal::Logs, OtelSignal::Metrics];
}

/// `event_name` is the record's own field, else its `event.name` attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct OtelLogRecord {
    pub event_name: String,
    pub severity_text: String,
    pub body: Option<Value>,
    pub attributes: OtelAttributes,
    pub resource: OtelAttributes,
    pub scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtelTemporality {
    Unspecified,
    Delta,
    Cumulative,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OtelNumber {
    Int(i64),
    Double(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OtelMetricData {
    Sum {
        temporality: OtelTemporality,
        is_monotonic: bool,
        value: Option<OtelNumber>,
    },
    Gauge {
        value: Option<OtelNumber>,
    },
    Histogram {
        temporality: OtelTemporality,
        count: u64,
        sum: Option<f64>,
    },
    ExponentialHistogram,
    Summary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OtelMetricPoint {
    pub name: String,
    pub data: OtelMetricData,
    pub attributes: OtelAttributes,
    pub resource: OtelAttributes,
    pub scope: String,
}

/// Header names are lowercase, one per value; an export from a test's own transport has none.
#[derive(Debug, Clone, PartialEq)]
pub struct OtelExport {
    pub signal: OtelSignal,
    pub headers: Vec<(String, String)>,
    pub body: OtelBody,
}

impl OtelExport {
    /// Case insensitive; the first value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtelBody {
    Kept(Vec<u8>),
    PastRetentionCap { len: usize },
    Unread(OtelUnreadBody),
}

/// `declared_len` is the request's `content-length`, `None` when absent or unreadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtelUnreadBody {
    Refused { declared_len: Option<usize> },
    Incomplete { declared_len: Option<usize> },
}

impl fmt::Display for OtelUnreadBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (outcome, declared_len) = match self {
            OtelUnreadBody::Refused { declared_len } => {
                ("refused as larger than the server's cap", declared_len)
            }
            OtelUnreadBody::Incomplete { declared_len } => ("cut short", declared_len),
        };
        match declared_len {
            Some(len) => write!(f, "{outcome}, content-length {len}"),
            None => write!(f, "{outcome}, no content-length"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OtelEvent {
    LogRecord(OtelLogRecord),
    Metric(OtelMetricPoint),
}

impl OtelEvent {
    pub fn signal(&self) -> OtelSignal {
        match self {
            OtelEvent::LogRecord(_) => OtelSignal::Logs,
            OtelEvent::Metric(_) => OtelSignal::Metrics,
        }
    }

    pub(crate) fn into_log_record(self) -> Option<OtelLogRecord> {
        match self {
            OtelEvent::LogRecord(record) => Some(record),
            OtelEvent::Metric(_) => None,
        }
    }

    pub(crate) fn into_metric_point(self) -> Option<OtelMetricPoint> {
        match self {
            OtelEvent::Metric(point) => Some(point),
            OtelEvent::LogRecord(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OtelDecodeError {
    #[error("content type {content_type:?} is not protobuf, the only encoding decoded")]
    UnsupportedContentType { content_type: String },
    #[error("protobuf: {error}")]
    Protobuf { error: String },
}

/// Any fault fails every later wait and the body text.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OtelFault {
    #[error("an OpenTelemetry {signal:?} body of {len} bytes did not decode: {error}")]
    Undecodable {
        signal: OtelSignal,
        len: usize,
        error: OtelDecodeError,
    },
    #[error("an OpenTelemetry {signal:?} body was {body}: {error}")]
    Unread {
        signal: OtelSignal,
        body: OtelUnreadBody,
        error: String,
    },
    #[error("a {method} {path:?} request, which the server does not serve")]
    Unserved { method: String, path: String },
}
