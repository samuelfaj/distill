//! OTLP bodies the recorder and server tests hand to the doubles, and what they decode to.

use std::collections::BTreeMap;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::metrics::v1::{
    Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, metric,
};
use prost::Message as _;

use crate::otel_event::{OtelEvent, OtelLogRecord, OtelMetricData, OtelMetricPoint};

pub(crate) fn logs_body(event_name: &str) -> Vec<u8> {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    event_name: event_name.to_owned(),
                    ..LogRecord::default()
                }],
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    }
    .encode_to_vec()
}

pub(crate) fn metrics_body(name: &str) -> Vec<u8> {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: name.to_owned(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint::default()],
                    })),
                    ..Metric::default()
                }],
                ..ScopeMetrics::default()
            }],
            ..ResourceMetrics::default()
        }],
    }
    .encode_to_vec()
}

pub(crate) fn logs_event(event_name: &str) -> OtelEvent {
    OtelEvent::LogRecord(OtelLogRecord {
        event_name: event_name.to_owned(),
        severity_text: String::new(),
        body: None,
        attributes: BTreeMap::new(),
        resource: BTreeMap::new(),
        scope: String::new(),
    })
}

pub(crate) fn metrics_event(name: &str) -> OtelEvent {
    OtelEvent::Metric(OtelMetricPoint {
        name: name.to_owned(),
        data: OtelMetricData::Gauge { value: None },
        attributes: BTreeMap::new(),
        resource: BTreeMap::new(),
        scope: String::new(),
    })
}
