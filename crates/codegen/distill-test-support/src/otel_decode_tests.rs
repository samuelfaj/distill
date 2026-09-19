use std::collections::BTreeMap;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, any_value,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum, metric,
    number_data_point,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message as _;
use serde_json::{Value, json};

use super::*;

fn attribute(key: &str, value: any_value::Value) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value: Some(value) }),
        ..KeyValue::default()
    }
}

fn string(value: &str) -> any_value::Value {
    any_value::Value::StringValue(value.to_owned())
}

fn logs_request(record: LogRecord) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![attribute("service.name", string("grok-cli"))],
                ..Resource::default()
            }),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope {
                    name: "grok_code".to_owned(),
                    ..InstrumentationScope::default()
                }),
                log_records: vec![record],
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    }
}

fn events_of(signal: OtelSignal, request: &impl prost::Message) -> Vec<OtelEvent> {
    decode_protobuf(signal, &request.encode_to_vec()).unwrap()
}

#[test]
fn log_record_decodes_with_attributes_resource_and_scope() {
    let record = LogRecord {
        event_name: "grok_code.user_prompt".to_owned(),
        severity_text: "INFO".to_owned(),
        body: Some(AnyValue {
            value: Some(string("prompt")),
        }),
        attributes: vec![
            attribute("session.id", string("s-1")),
            attribute("event.sequence", any_value::Value::IntValue(7)),
            attribute("prompt.redacted", any_value::Value::BoolValue(true)),
        ],
        ..LogRecord::default()
    };

    let events = events_of(OtelSignal::Logs, &logs_request(record));

    assert_eq!(
        vec![OtelEvent::LogRecord(OtelLogRecord {
            event_name: "grok_code.user_prompt".to_owned(),
            severity_text: "INFO".to_owned(),
            body: Some(Value::from("prompt")),
            attributes: BTreeMap::from([
                ("session.id".to_owned(), Value::from("s-1")),
                ("event.sequence".to_owned(), Value::from(7)),
                ("prompt.redacted".to_owned(), Value::from(true)),
            ]),
            resource: BTreeMap::from([("service.name".to_owned(), Value::from("grok-cli"))]),
            scope: "grok_code".to_owned(),
        })],
        events
    );
}

#[test]
fn event_name_falls_back_to_the_event_name_attribute() {
    let record = LogRecord {
        attributes: vec![attribute("event.name", string("grok_code.session_start"))],
        ..LogRecord::default()
    };

    let events = events_of(OtelSignal::Logs, &logs_request(record));

    let [OtelEvent::LogRecord(record)] = events.as_slice() else {
        panic!("expected one log record, got {events:?}");
    };
    assert_eq!("grok_code.session_start", record.event_name);
}

#[test]
fn nested_attribute_values_flatten_to_json() {
    let record = LogRecord {
        attributes: vec![
            attribute(
                "list",
                any_value::Value::ArrayValue(ArrayValue {
                    values: vec![AnyValue {
                        value: Some(any_value::Value::DoubleValue(1.5)),
                    }],
                }),
            ),
            attribute("bytes", any_value::Value::BytesValue(vec![0, 255])),
        ],
        ..LogRecord::default()
    };

    let events = events_of(OtelSignal::Logs, &logs_request(record));

    let [OtelEvent::LogRecord(record)] = events.as_slice() else {
        panic!("expected one log record, got {events:?}");
    };
    assert_eq!(
        BTreeMap::from([
            ("list".to_owned(), json!([1.5])),
            ("bytes".to_owned(), json!("AP8=")),
        ]),
        record.attributes
    );
}

#[test]
fn sum_metric_point_carries_temporality_monotonicity_and_value() {
    let request = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: None,
            scope_metrics: vec![ScopeMetrics {
                scope: Some(InstrumentationScope {
                    name: "grok_code".to_owned(),
                    ..InstrumentationScope::default()
                }),
                metrics: vec![Metric {
                    name: "grok_code.turn.count".to_owned(),
                    data: Some(metric::Data::Sum(Sum {
                        data_points: vec![NumberDataPoint {
                            attributes: vec![attribute("outcome", string("ok"))],
                            value: Some(number_data_point::Value::AsInt(3)),
                            ..NumberDataPoint::default()
                        }],
                        aggregation_temporality: i32::from(AggregationTemporality::Delta),
                        is_monotonic: true,
                    })),
                    ..Metric::default()
                }],
                ..ScopeMetrics::default()
            }],
            ..ResourceMetrics::default()
        }],
    };

    let events = events_of(OtelSignal::Metrics, &request);

    assert_eq!(
        vec![OtelEvent::Metric(OtelMetricPoint {
            name: "grok_code.turn.count".to_owned(),
            data: OtelMetricData::Sum {
                temporality: OtelTemporality::Delta,
                is_monotonic: true,
                value: Some(OtelNumber::Int(3)),
            },
            attributes: BTreeMap::from([("outcome".to_owned(), Value::from("ok"))]),
            resource: BTreeMap::new(),
            scope: "grok_code".to_owned(),
        })],
        events
    );
}

#[test]
fn decode_post_accepts_protobuf_media_types_only() {
    let request = logs_request(LogRecord::default());
    let body = request.encode_to_vec();
    let events = events_of(OtelSignal::Logs, &request);
    let cases = [
        ("application/x-protobuf", Ok(events.clone())),
        ("Application/Protobuf; charset=binary", Ok(events)),
        (
            "application/json",
            Err(OtelDecodeError::UnsupportedContentType {
                content_type: "application/json".to_owned(),
            }),
        ),
    ];

    for (content_type, expected) in cases {
        assert_eq!(
            expected,
            decode_post(OtelSignal::Logs, content_type, &body),
            "{content_type}"
        );
    }
}
