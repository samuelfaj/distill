//! Decodes one OTLP export into the typed events the recorder keeps. The content type belongs to
//! the HTTP post, so only `decode_post` checks it; `decode_protobuf` takes the message alone.

use base64::Engine as _;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, Metric, metric, number_data_point,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message as _;
use serde_json::Value;

use crate::otel_event::{
    OtelAttributes, OtelDecodeError, OtelEvent, OtelLogRecord, OtelMetricData, OtelMetricPoint,
    OtelNumber, OtelSignal, OtelTemporality,
};

/// OTLP/HTTP allows protobuf or JSON; both grok exporters post protobuf, so only it is decoded.
const PROTOBUF_MEDIA_TYPES: [&str; 2] = ["application/x-protobuf", "application/protobuf"];

pub(crate) fn decode_post(
    signal: OtelSignal,
    content_type: &str,
    body: &[u8],
) -> Result<Vec<OtelEvent>, OtelDecodeError> {
    let media_type = content_type
        .split_once(';')
        .map_or(content_type, |(media_type, _)| media_type)
        .trim()
        .to_ascii_lowercase();
    if !PROTOBUF_MEDIA_TYPES.contains(&media_type.as_str()) {
        return Err(OtelDecodeError::UnsupportedContentType {
            content_type: content_type.to_owned(),
        });
    }
    decode_protobuf(signal, body)
}

pub(crate) fn decode_protobuf(
    signal: OtelSignal,
    message: &[u8],
) -> Result<Vec<OtelEvent>, OtelDecodeError> {
    let decoded = match signal {
        OtelSignal::Logs => {
            ExportLogsServiceRequest::decode(message).map(|request| log_records(&request))
        }
        OtelSignal::Metrics => {
            ExportMetricsServiceRequest::decode(message).map(|request| metric_points(&request))
        }
    };
    decoded.map_err(|error| OtelDecodeError::Protobuf {
        error: error.to_string(),
    })
}

fn log_records(request: &ExportLogsServiceRequest) -> Vec<OtelEvent> {
    let mut events = Vec::new();
    for resource_logs in &request.resource_logs {
        let resource = resource_attributes(resource_logs.resource.as_ref());
        for scope_logs in &resource_logs.scope_logs {
            let scope = scope_name(scope_logs.scope.as_ref());
            for record in &scope_logs.log_records {
                let attributes = attributes_to_json(&record.attributes);
                let event_name = if record.event_name.is_empty() {
                    attributes
                        .get("event.name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    record.event_name.clone()
                };
                events.push(OtelEvent::LogRecord(OtelLogRecord {
                    event_name,
                    severity_text: record.severity_text.clone(),
                    body: record.body.as_ref().map(any_value_to_json),
                    attributes,
                    resource: resource.clone(),
                    scope: scope.clone(),
                }));
            }
        }
    }
    events
}

fn metric_points(request: &ExportMetricsServiceRequest) -> Vec<OtelEvent> {
    let mut events = Vec::new();
    for resource_metrics in &request.resource_metrics {
        let resource = resource_attributes(resource_metrics.resource.as_ref());
        for scope_metrics in &resource_metrics.scope_metrics {
            let scope = scope_name(scope_metrics.scope.as_ref());
            for metric in &scope_metrics.metrics {
                events.extend(metric_data_points(metric, &resource, &scope));
            }
        }
    }
    events
}

fn metric_data_points(metric: &Metric, resource: &OtelAttributes, scope: &str) -> Vec<OtelEvent> {
    let metric_point = |data: OtelMetricData, attributes: &[KeyValue]| {
        OtelEvent::Metric(OtelMetricPoint {
            name: metric.name.clone(),
            data,
            attributes: attributes_to_json(attributes),
            resource: resource.clone(),
            scope: scope.to_owned(),
        })
    };
    match &metric.data {
        Some(metric::Data::Sum(sum)) => {
            let temporality = OtelTemporality::from(sum.aggregation_temporality());
            sum.data_points
                .iter()
                .map(|data_point| {
                    let data = OtelMetricData::Sum {
                        temporality,
                        is_monotonic: sum.is_monotonic,
                        value: data_point.value.map(OtelNumber::from),
                    };
                    metric_point(data, &data_point.attributes)
                })
                .collect()
        }
        Some(metric::Data::Gauge(gauge)) => gauge
            .data_points
            .iter()
            .map(|data_point| {
                let data = OtelMetricData::Gauge {
                    value: data_point.value.map(OtelNumber::from),
                };
                metric_point(data, &data_point.attributes)
            })
            .collect(),
        Some(metric::Data::Histogram(histogram)) => {
            let temporality = OtelTemporality::from(histogram.aggregation_temporality());
            histogram
                .data_points
                .iter()
                .map(|data_point| {
                    let data = OtelMetricData::Histogram {
                        temporality,
                        count: data_point.count,
                        sum: data_point.sum,
                    };
                    metric_point(data, &data_point.attributes)
                })
                .collect()
        }
        Some(metric::Data::ExponentialHistogram(histogram)) => histogram
            .data_points
            .iter()
            .map(|data_point| {
                metric_point(OtelMetricData::ExponentialHistogram, &data_point.attributes)
            })
            .collect(),
        Some(metric::Data::Summary(summary)) => summary
            .data_points
            .iter()
            .map(|data_point| metric_point(OtelMetricData::Summary, &data_point.attributes))
            .collect(),
        None => Vec::new(),
    }
}

impl From<number_data_point::Value> for OtelNumber {
    fn from(value: number_data_point::Value) -> OtelNumber {
        match value {
            number_data_point::Value::AsInt(int) => OtelNumber::Int(int),
            number_data_point::Value::AsDouble(double) => OtelNumber::Double(double),
        }
    }
}

impl From<AggregationTemporality> for OtelTemporality {
    fn from(temporality: AggregationTemporality) -> OtelTemporality {
        match temporality {
            AggregationTemporality::Unspecified => OtelTemporality::Unspecified,
            AggregationTemporality::Delta => OtelTemporality::Delta,
            AggregationTemporality::Cumulative => OtelTemporality::Cumulative,
        }
    }
}

fn resource_attributes(resource: Option<&Resource>) -> OtelAttributes {
    resource
        .map(|resource| attributes_to_json(&resource.attributes))
        .unwrap_or_default()
}

fn scope_name(scope: Option<&InstrumentationScope>) -> String {
    scope.map(|scope| scope.name.clone()).unwrap_or_default()
}

fn attributes_to_json(attributes: &[KeyValue]) -> OtelAttributes {
    attributes
        .iter()
        .map(|pair| {
            let value = pair.value.as_ref().map_or(Value::Null, any_value_to_json);
            (pair.key.clone(), value)
        })
        .collect()
}

/// Bytes become standard base64, as OTLP/JSON encodes them. A string table index (the profiles
/// signal's string form) is kept as `{"string_index": n}`; no route here carries a string table.
fn any_value_to_json(value: &AnyValue) -> Value {
    match &value.value {
        Some(any_value::Value::StringValue(text)) => Value::from(text.as_str()),
        Some(any_value::Value::StringValueStrindex(index)) => {
            serde_json::json!({ "string_index": index })
        }
        Some(any_value::Value::BoolValue(flag)) => Value::from(*flag),
        Some(any_value::Value::IntValue(int)) => Value::from(*int),
        Some(any_value::Value::DoubleValue(double)) => Value::from(*double),
        Some(any_value::Value::ArrayValue(array)) => {
            Value::Array(array.values.iter().map(any_value_to_json).collect())
        }
        Some(any_value::Value::KvlistValue(list)) => {
            Value::Object(attributes_to_json(&list.values).into_iter().collect())
        }
        Some(any_value::Value::BytesValue(bytes)) => {
            Value::from(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        None => Value::Null,
    }
}

#[cfg(test)]
#[path = "otel_decode_tests.rs"]
mod tests;
