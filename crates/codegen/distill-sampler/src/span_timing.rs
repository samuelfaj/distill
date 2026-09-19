// Modified for Distill by Samuel Fajreldines, 2026.
//! Record stream timings on the span handle; a disabled span never becomes current.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use futures_util::stream::BoxStream;
use tokio::time::Instant;

use distill_sampling_types::Result;

macro_rules! stream_span {
    ($name:expr $(, $($field:tt)+)?) => {
        $crate::span_timing::Region::from_span(tracing::info_span!(
            $name,
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
            request_build_us = tracing::field::Empty,
            response_headers_ms = tracing::field::Empty,
            stream_setup_us = tracing::field::Empty,
            ttft_ms = tracing::field::Empty,
            ttft_outcome = tracing::field::Empty,
            $($($field)+)?
        ))
    };
}
pub(crate) use stream_span;

const REQUEST_BUILD_US: &str = "request_build_us";
const RESPONSE_HEADERS_MS: &str = "response_headers_ms";
const STREAM_SETUP_US: &str = "stream_setup_us";
const TTFT_MS: &str = "ttft_ms";
const TTFT_OUTCOME: &str = "ttft_outcome";

pub(crate) const STATUS_CODE: &str = "status_code";
pub(crate) const SUCCESS: &str = "success";
pub(crate) const ERROR: &str = "error";

#[cfg(test)]
pub(crate) const TIMING_FIELDS: [&str; 5] = [
    REQUEST_BUILD_US,
    RESPONSE_HEADERS_MS,
    STREAM_SETUP_US,
    TTFT_MS,
    TTFT_OUTCOME,
];

/// Sampler cannot depend on `distill-telemetry` (cycle); held, not entered.
/// Never entered: safe across `.await`; close belongs to scope.
#[must_use = "dropping a Region immediately closes its span as a zero-length frame"]
pub(crate) struct Region(tracing::Span);

impl Region {
    pub(crate) fn from_span(span: tracing::Span) -> Self {
        Self(span)
    }

    pub(crate) fn span(&self) -> &tracing::Span {
        &self.0
    }

    fn close(self) {}
}

pub(crate) enum ItemClass {
    Content,
    Error,
    End,
    Other,
}

#[derive(PartialEq)]
enum Stage {
    Start,
    RequestBuilt,
    HeadersRead,
}

struct SegmentNames {
    request_build: &'static str,
    response_headers: &'static str,
    stream_setup: &'static str,
    ttft: &'static str,
}

impl SegmentNames {
    fn generic() -> Self {
        Self {
            request_build: "http.stream.request_build",
            response_headers: "http.stream.response_headers",
            stream_setup: "http.stream.stream_setup",
            ttft: "http.stream.ttft",
        }
    }

    fn for_parent(parent: &tracing::Span) -> Self {
        match parent.metadata().map(|meta| meta.name()) {
            Some("http.chat_completion_stream") => Self {
                request_build: "http.chat_completion_stream.request_build",
                response_headers: "http.chat_completion_stream.response_headers",
                stream_setup: "http.chat_completion_stream.stream_setup",
                ttft: "http.chat_completion_stream.ttft",
            },
            Some("http.create_response_stream") => Self {
                request_build: "http.create_response_stream.request_build",
                response_headers: "http.create_response_stream.response_headers",
                stream_setup: "http.create_response_stream.stream_setup",
                ttft: "http.create_response_stream.ttft",
            },
            Some("http.create_message_stream") => Self {
                request_build: "http.create_message_stream.request_build",
                response_headers: "http.create_message_stream.response_headers",
                stream_setup: "http.create_message_stream.stream_setup",
                ttft: "http.create_message_stream.ttft",
            },
            _ => Self::generic(),
        }
    }
}

#[must_use]
pub(crate) struct StreamSpanTiming {
    region: Option<Region>,
    handle: tracing::Span,
    segment: Option<Region>,
    names: SegmentNames,
    last_mark_at: Instant,
    stage: Stage,
}

impl StreamSpanTiming {
    pub(crate) fn start(region: Region) -> Self {
        if region.span().is_disabled() {
            drop(region);
            return Self::inert();
        }
        let names = SegmentNames::for_parent(region.span());
        let handle = region.span().clone();
        let mut this = Self {
            region: Some(region),
            handle,
            segment: None,
            names,
            last_mark_at: Instant::now(),
            stage: Stage::Start,
        };
        let request_build = this.names.request_build;
        this.open_segment(request_build);
        this
    }

    fn inert() -> Self {
        Self {
            region: None,
            handle: tracing::Span::none(),
            segment: None,
            names: SegmentNames::generic(),
            last_mark_at: Instant::now(),
            stage: Stage::Start,
        }
    }

    pub(crate) fn span(&self) -> &tracing::Span {
        &self.handle
    }

    pub(crate) fn record_transport_failure(&self, error: &str) {
        let span = self.span();
        span.record(SUCCESS, false);
        span.record(ERROR, error);
    }

    pub(crate) fn record_request_build(&mut self) {
        if self.region.is_none() {
            return;
        }
        debug_assert!(
            self.stage == Stage::Start,
            "record_request_build must run first"
        );
        self.stage = Stage::RequestBuilt;
        self.close_segment();
        self.mark(REQUEST_BUILD_US, elapsed_us);
        let response_headers = self.names.response_headers;
        self.open_segment(response_headers);
    }

    pub(crate) fn record_response_headers(&mut self) {
        if self.region.is_none() {
            return;
        }
        debug_assert!(
            self.stage == Stage::RequestBuilt,
            "record_response_headers must run after record_request_build"
        );
        self.stage = Stage::HeadersRead;
        self.close_segment();
        self.mark(RESPONSE_HEADERS_MS, elapsed_ms);
        let stream_setup = self.names.stream_setup;
        self.open_segment(stream_setup);
    }

    #[must_use]
    pub(crate) fn hold_until_first_content<T>(
        mut self,
        stream: BoxStream<'static, Result<T>>,
        classify: fn(&T) -> ItemClass,
    ) -> BoxStream<'static, Result<T>>
    where
        T: Send + 'static,
    {
        if self.region.is_none() {
            return stream;
        }
        debug_assert!(
            self.stage == Stage::HeadersRead,
            "hold_until_first_content must run after the request build and response header marks"
        );
        self.close_segment();
        self.mark(STREAM_SETUP_US, elapsed_us);
        let Some(region) = self.region.take() else {
            return stream;
        };
        self.handle = tracing::Span::none();
        let ttft = self.names.ttft;
        let segment = open_segment(region.span(), ttft);
        let returned_at = self.last_mark_at;
        Box::pin(HoldUntilContentStream {
            inner: stream,
            held: Some(HeldSpan {
                segment: Some(segment),
                region,
                returned_at,
            }),
            classify,
        })
    }

    fn open_segment(&mut self, name: &'static str) {
        let Some(region) = self.region.as_ref() else {
            return;
        };
        self.segment = Some(open_segment(region.span(), name));
    }

    fn close_segment(&mut self) {
        if let Some(segment) = self.segment.take() {
            segment.close();
        }
    }

    fn mark(&mut self, field: &str, elapsed: fn(Instant, Instant) -> i64) {
        let now = Instant::now();
        self.span().record(field, elapsed(self.last_mark_at, now));
        self.last_mark_at = now;
    }
}

impl Drop for StreamSpanTiming {
    fn drop(&mut self) {
        let Some(region) = self.region.take() else {
            return;
        };
        self.close_segment();
        let outcome = match self.stage {
            Stage::Start => TtftOutcome::RequestError,
            Stage::RequestBuilt | Stage::HeadersRead => TtftOutcome::HttpError,
        };
        region.span().record(TTFT_OUTCOME, outcome.as_ref());
        region.close();
    }
}

fn open_segment(parent: &tracing::Span, name: &'static str) -> Region {
    if parent.is_disabled() {
        return Region::from_span(tracing::Span::none());
    }
    let span = match name {
        "http.stream.request_build" => {
            tracing::info_span!(parent: parent, "http.stream.request_build")
        }
        "http.stream.response_headers" => {
            tracing::info_span!(parent: parent, "http.stream.response_headers")
        }
        "http.stream.stream_setup" => {
            tracing::info_span!(parent: parent, "http.stream.stream_setup")
        }
        "http.stream.ttft" => tracing::info_span!(parent: parent, "http.stream.ttft"),
        "http.chat_completion_stream.request_build" => {
            tracing::info_span!(parent: parent, "http.chat_completion_stream.request_build")
        }
        "http.chat_completion_stream.response_headers" => {
            tracing::info_span!(parent: parent, "http.chat_completion_stream.response_headers")
        }
        "http.chat_completion_stream.stream_setup" => {
            tracing::info_span!(parent: parent, "http.chat_completion_stream.stream_setup")
        }
        "http.chat_completion_stream.ttft" => {
            tracing::info_span!(parent: parent, "http.chat_completion_stream.ttft")
        }
        "http.create_response_stream.request_build" => {
            tracing::info_span!(parent: parent, "http.create_response_stream.request_build")
        }
        "http.create_response_stream.response_headers" => {
            tracing::info_span!(parent: parent, "http.create_response_stream.response_headers")
        }
        "http.create_response_stream.stream_setup" => {
            tracing::info_span!(parent: parent, "http.create_response_stream.stream_setup")
        }
        "http.create_response_stream.ttft" => {
            tracing::info_span!(parent: parent, "http.create_response_stream.ttft")
        }
        "http.create_message_stream.request_build" => {
            tracing::info_span!(parent: parent, "http.create_message_stream.request_build")
        }
        "http.create_message_stream.response_headers" => {
            tracing::info_span!(parent: parent, "http.create_message_stream.response_headers")
        }
        "http.create_message_stream.stream_setup" => {
            tracing::info_span!(parent: parent, "http.create_message_stream.stream_setup")
        }
        "http.create_message_stream.ttft" => {
            tracing::info_span!(parent: parent, "http.create_message_stream.ttft")
        }
        other => {
            debug_assert!(false, "unknown stream segment {other}");
            tracing::Span::none()
        }
    };
    Region::from_span(span)
}

struct HoldUntilContentStream<T> {
    inner: BoxStream<'static, Result<T>>,
    held: Option<HeldSpan>,
    classify: fn(&T) -> ItemClass,
}

struct HeldSpan {
    // Closed before `region` so the profiler's parent self-time subtracts this child.
    segment: Option<Region>,
    region: Region,
    returned_at: Instant,
}

#[derive(Clone, Copy, PartialEq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum TtftOutcome {
    Content,
    Error,
    EndOfStream,
    Dropped,
    RequestError,
    HttpError,
}

impl HeldSpan {
    fn release(mut self, outcome: TtftOutcome) {
        self.region.span().record(TTFT_OUTCOME, outcome.as_ref());
        if let Some(segment) = self.segment.take() {
            segment.close();
        }
        self.region.close();
    }
}

impl<T> Stream for HoldUntilContentStream<T> {
    type Item = Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Poll::Ready(ready) = this.inner.as_mut().poll_next(cx) else {
            return Poll::Pending;
        };
        if this.held.is_none() {
            return Poll::Ready(ready);
        }
        let outcome = match &ready {
            Some(Ok(item)) => match (this.classify)(item) {
                ItemClass::Content => Some(TtftOutcome::Content),
                ItemClass::Error => Some(TtftOutcome::Error),
                ItemClass::End => Some(TtftOutcome::EndOfStream),
                ItemClass::Other => None,
            },
            Some(Err(_)) => Some(TtftOutcome::Error),
            None => Some(TtftOutcome::EndOfStream),
        };
        let Some(outcome) = outcome else {
            return Poll::Ready(ready);
        };
        let Some(held) = this.held.take() else {
            return Poll::Ready(ready);
        };
        if outcome == TtftOutcome::Content {
            held.region
                .span()
                .record(TTFT_MS, elapsed_ms(held.returned_at, Instant::now()));
        }
        held.release(outcome);
        Poll::Ready(ready)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<T> Drop for HoldUntilContentStream<T> {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            held.release(TtftOutcome::Dropped);
        }
    }
}

// `i64` not `u64`: the OTLP redactor stringifies `u64` attributes and drops them.
fn elapsed_ms(from: Instant, to: Instant) -> i64 {
    saturating_i64(to.saturating_duration_since(from).as_millis())
}

fn elapsed_us(from: Instant, to: Instant) -> i64 {
    saturating_i64(to.saturating_duration_since(from).as_micros())
}

fn saturating_i64(value: u128) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "span_timing_tests.rs"]
mod tests;
