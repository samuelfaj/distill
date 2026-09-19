use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use super::*;

fn is_interior_segment(name: &str) -> bool {
    name.ends_with(".request_build")
        || name.ends_with(".response_headers")
        || name.ends_with(".stream_setup")
        || name.ends_with(".ttft")
}

#[derive(Clone, Default)]
struct SpanProbe {
    spans: Arc<Mutex<Vec<ProbedSpan>>>,
}

#[derive(Default)]
struct ProbedSpan {
    id: u64,
    name: &'static str,
    closed: bool,
    fields: HashMap<String, String>,
    kinds: HashMap<String, &'static str>,
}

impl SpanProbe {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
    }

    fn stream_span(&self) -> Option<ProbedSpan> {
        let spans = self.spans.lock().unwrap();
        spans
            .iter()
            .find(|span| span.name == "stream_request")
            .cloned()
    }

    fn is_closed(&self) -> bool {
        let spans = self.spans.lock().unwrap();
        let owners: Vec<_> = spans
            .iter()
            .filter(|span| !is_interior_segment(span.name))
            .collect();
        assert!(
            !owners.is_empty(),
            "expected the stream or parent span to be registered"
        );
        owners.iter().all(|span| span.closed)
    }

    fn field(&self, name: &str) -> Option<String> {
        self.stream_span()
            .and_then(|span| span.fields.get(name).cloned())
    }

    fn field_kind(&self, name: &str) -> Option<&'static str> {
        self.stream_span()
            .and_then(|span| span.kinds.get(name).copied())
    }

    fn span_names(&self) -> Vec<&'static str> {
        self.spans
            .lock()
            .unwrap()
            .iter()
            .map(|span| span.name)
            .collect()
    }

    fn with_open_entry(&self, id: &tracing::span::Id, update: impl FnOnce(&mut ProbedSpan)) {
        let mut spans = self.spans.lock().unwrap();
        if let Some(span) = spans
            .iter_mut()
            .rev()
            .find(|span| span.id == id.into_u64() && !span.closed)
        {
            update(span);
        }
    }
}

impl Clone for ProbedSpan {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            name: self.name,
            closed: self.closed,
            fields: self.fields.clone(),
            kinds: self.kinds.clone(),
        }
    }
}

impl<S> Layer<S> for SpanProbe
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let mut span = ProbedSpan {
            id: id.into_u64(),
            name: attrs.metadata().name(),
            ..Default::default()
        };
        attrs.record(&mut FieldRecorder {
            fields: &mut span.fields,
            kinds: &mut span.kinds,
        });
        self.spans.lock().unwrap().push(span);
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: Context<'_, S>,
    ) {
        self.with_open_entry(id, |span| {
            values.record(&mut FieldRecorder {
                fields: &mut span.fields,
                kinds: &mut span.kinds,
            })
        });
    }

    fn on_close(&self, id: tracing::span::Id, _ctx: Context<'_, S>) {
        self.with_open_entry(&id, |span| span.closed = true);
    }
}

struct FieldRecorder<'a> {
    fields: &'a mut HashMap<String, String>,
    kinds: &'a mut HashMap<String, &'static str>,
}

impl FieldRecorder<'_> {
    fn put(&mut self, field: &tracing::field::Field, value: String, kind: &'static str) {
        self.fields.insert(field.name().to_owned(), value);
        self.kinds.insert(field.name().to_owned(), kind);
    }
}

impl tracing::field::Visit for FieldRecorder<'_> {
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.put(field, value.to_string(), "i64");
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.put(field, value.to_string(), "u64");
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.put(field, value.to_owned(), "str");
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.put(field, format!("{value:?}"), "debug");
    }
}

fn stream_region() -> Region {
    stream_span!("stream_request")
}

fn primed_timing() -> StreamSpanTiming {
    let mut timing = StreamSpanTiming::start(stream_region());
    timing.record_request_build();
    timing.record_response_headers();
    timing
}

const TIMING_NUMERIC_FIELDS: [&str; 4] = [
    REQUEST_BUILD_US,
    RESPONSE_HEADERS_MS,
    STREAM_SETUP_US,
    TTFT_MS,
];

fn all_content(_: &u8) -> ItemClass {
    ItemClass::Content
}

#[tokio::test]
async fn span_stays_open_until_first_content_and_records_segments() {
    let probe = SpanProbe::default();
    let _guard = probe.install();

    let items: Vec<Result<u8>> = vec![Ok(1), Ok(2)];
    let mut wrapped =
        primed_timing().hold_until_first_content(stream::iter(items).boxed(), all_content);
    assert!(
        !probe.is_closed(),
        "span must stay open until the first content item"
    );

    assert_eq!(wrapped.next().await.unwrap().unwrap(), 1);
    assert!(
        probe.is_closed(),
        "first content item must release the span"
    );
    for field in TIMING_FIELDS {
        assert!(probe.field(field).is_some(), "{field} not recorded");
    }
    for field in TIMING_NUMERIC_FIELDS {
        assert_eq!(
            probe.field_kind(field),
            Some("i64"),
            "{field} must record as i64"
        );
    }
    assert_eq!(probe.field(TTFT_OUTCOME).as_deref(), Some("content"));
    let names = probe.span_names();
    for expected in [
        "http.stream.request_build",
        "http.stream.response_headers",
        "http.stream.stream_setup",
        "http.stream.ttft",
    ] {
        assert!(names.contains(&expected), "missing segment span {expected}");
    }
    assert!(
        !names.contains(&"http.stream.segment"),
        "interior segments must not share http.stream.segment"
    );

    assert_eq!(wrapped.next().await.unwrap().unwrap(), 2);
    assert!(wrapped.next().await.is_none());
}

#[derive(Clone, Copy, Debug)]
enum OutcomeScenario {
    ErrorItem,
    DropBeforeItem,
}

#[tokio::test]
async fn non_content_release_records_ttft_outcome() {
    for (scenario, expected) in [
        (OutcomeScenario::ErrorItem, "error"),
        (OutcomeScenario::DropBeforeItem, "dropped"),
    ] {
        let probe = SpanProbe::default();
        let _guard = probe.install();

        match scenario {
            OutcomeScenario::ErrorItem => {
                let items: Vec<Result<u8>> = vec![Ok(0), Ok(9)];
                let mut wrapped = primed_timing().hold_until_first_content(
                    stream::iter(items).boxed(),
                    |value: &u8| match *value {
                        9 => ItemClass::Error,
                        0 => ItemClass::Other,
                        _ => ItemClass::Content,
                    },
                );
                assert_eq!(wrapped.next().await.unwrap().unwrap(), 0);
                assert!(!probe.is_closed(), "{scenario:?}");
                assert_eq!(wrapped.next().await.unwrap().unwrap(), 9);
            }
            OutcomeScenario::DropBeforeItem => {
                let wrapped = primed_timing()
                    .hold_until_first_content(stream::pending::<Result<u8>>().boxed(), all_content);
                assert!(!probe.is_closed(), "{scenario:?}");
                drop(wrapped);
            }
        }

        assert!(probe.is_closed(), "{scenario:?}");
        assert_eq!(
            probe.field(TTFT_OUTCOME).as_deref(),
            Some(expected),
            "{scenario:?}"
        );
        assert_eq!(probe.field(TTFT_MS), None, "{scenario:?}");
    }
}

#[tokio::test]
async fn disabled_span_handle_does_not_touch_current_parent() {
    let probe = SpanProbe::default();
    let _guard = probe.install();

    let parent = tracing::info_span!("disabled_span_parent");
    let mut wrapped = {
        let _entered = parent.enter();
        let mut timing = StreamSpanTiming::start(Region::from_span(tracing::Span::none()));
        timing.record_request_build();
        timing.record_response_headers();
        let items: Vec<Result<u8>> = vec![Ok(1)];
        timing.hold_until_first_content(stream::iter(items).boxed(), all_content)
    };

    drop(parent);
    assert!(
        probe.is_closed(),
        "a disabled stream span must not hold the current parent open"
    );

    assert_eq!(wrapped.next().await.unwrap().unwrap(), 1);

    for field in TIMING_FIELDS {
        assert_eq!(probe.field(field), None, "{field} leaked onto the parent");
    }
    for field in [STATUS_CODE, SUCCESS, ERROR] {
        assert_eq!(probe.field(field), None, "{field} leaked onto the parent");
    }
}
