use std::time::{Duration, Instant};

use super::first_use::{Freshness, classify_first_use};
use super::{
    MAX_TRACKED_ORIGINS, PrewarmOutcome, WarmLookup, WarmState, endpoint_origin,
    has_room_after_sweep, note_first_sampling_use, should_dial, warm_state,
};

#[test]
fn warm_state_machine() {
    assert_eq!(
        endpoint_origin("https://api.x.ai/v1?api-version=x"),
        Some("https://api.x.ai".to_string()),
        "origin phase: path and query are stripped to the dialable origin"
    );
    assert_eq!(
        endpoint_origin("http://127.0.0.1:8080/v1/"),
        Some("http://127.0.0.1:8080".to_string()),
        "origin phase: the port is part of the origin"
    );
    for undialable in ["not a url", "data:text/plain,hi", "foo://bar/v1"] {
        assert_eq!(
            endpoint_origin(undialable),
            None,
            "origin phase: {undialable:?} has no dialable origin"
        );
    }

    let idle = Duration::from_secs(90);
    let base = Instant::now();
    assert!(
        should_dial(None, base, idle),
        "gate phase: an untracked origin dials"
    );
    assert!(
        !should_dial(Some(&WarmState::InFlight), base, idle),
        "gate phase: an in-flight dial blocks a second dial"
    );
    assert!(
        !should_dial(Some(&WarmState::Warmed(base)), base, idle),
        "gate phase: a fresh warm blocks a re-dial"
    );
    assert!(
        should_dial(Some(&WarmState::Warmed(base)), base + idle, idle),
        "gate phase: a warm at the idle window re-dials"
    );
    let later = base + idle + Duration::from_secs(1);
    assert!(
        should_dial(Some(&WarmState::Warmed(base)), later, idle),
        "gate phase: a warm past the idle window re-dials"
    );

    let now = base + idle;
    let mut state = std::collections::HashMap::new();
    state.insert("in-flight".to_string(), WarmState::InFlight);
    state.insert("stale".to_string(), WarmState::Warmed(base));
    state.insert("fresh".to_string(), WarmState::Warmed(now));
    assert!(
        has_room_after_sweep(&mut state, now, idle),
        "sweep phase: evicting the stale warm frees a slot"
    );
    assert!(
        !state.contains_key("stale"),
        "sweep phase: a warm at the idle window frees its slot"
    );
    assert!(
        state.contains_key("in-flight"),
        "sweep phase: an in-flight dial must survive the sweep"
    );
    assert!(
        state.contains_key("fresh"),
        "sweep phase: a fresh warm must survive the sweep"
    );
    for i in state.len()..MAX_TRACKED_ORIGINS {
        state.insert(format!("live-{i}"), WarmState::Warmed(now));
    }
    assert!(
        !has_room_after_sweep(&mut state, now, idle),
        "sweep phase: a tracker full of live entries has no room"
    );

    for (lookup, expected, invariant) in [
        (
            WarmLookup::Warmed(idle - Duration::from_secs(1)),
            (Freshness::Fresh, Some(89_000)),
            "a warm younger than the idle window is fresh, age in reportable millis",
        ),
        (
            WarmLookup::Warmed(idle),
            (Freshness::Stale, Some(90_000)),
            "the idle window itself is stale",
        ),
        (
            WarmLookup::InFlight,
            (Freshness::Pending, None),
            "an in-flight dial is pending with no age",
        ),
        (
            WarmLookup::Absent,
            (Freshness::Absent, None),
            "an untracked origin is absent with no age",
        ),
    ] {
        assert_eq!(
            classify_first_use(lookup, idle),
            expected,
            "classify phase: {invariant}"
        );
    }
}

#[test]
fn status_strings_are_stable() {
    for (outcome, status) in [
        (PrewarmOutcome::SharingDisabled, "sharing_disabled"),
        (PrewarmOutcome::NoOrigin, "no_origin"),
        (PrewarmOutcome::ClientUnavailable, "client_unavailable"),
        (PrewarmOutcome::AlreadyClaimed, "already_claimed"),
        (PrewarmOutcome::TrackerFull, "tracker_full"),
        (PrewarmOutcome::Warmed, "warmed"),
        (PrewarmOutcome::Truncated, "truncated"),
        (PrewarmOutcome::Failed, "failed"),
        (PrewarmOutcome::TimedOut, "timed_out"),
    ] {
        assert_eq!(<&'static str>::from(outcome), status);
    }
    for (freshness, status) in [
        (Freshness::Fresh, "warm_fresh"),
        (Freshness::Stale, "warm_stale"),
        (Freshness::Pending, "warm_pending"),
        (Freshness::Absent, "warm_absent"),
    ] {
        assert_eq!(<&'static str>::from(freshness), status);
    }
}

mod span_capture {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Default)]
    pub(super) struct Fields {
        pub(super) strs: BTreeMap<String, String>,
        pub(super) ints: BTreeMap<String, i64>,
        pub(super) span_count: usize,
    }

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.strs
                .insert(field.name().to_string(), value.to_string());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.ints.insert(field.name().to_string(), value);
        }
        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    pub(super) struct Capture {
        span_name: &'static str,
        pub(super) fields: Mutex<Fields>,
        next_id: AtomicU64,
    }

    impl Capture {
        pub(super) fn new(span_name: &'static str) -> Self {
            Self {
                span_name,
                fields: Mutex::new(Fields::default()),
                next_id: AtomicU64::new(1),
            }
        }
    }

    impl Subscriber for Capture {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, span: &Attributes<'_>) -> Id {
            if span.metadata().name() == self.span_name {
                let mut fields = self.fields.lock().unwrap();
                fields.span_count += 1;
                span.record(&mut *fields);
            }
            Id::from_u64(self.next_id.fetch_add(1, Ordering::Relaxed))
        }
        fn record(&self, _span: &Id, values: &Record<'_>) {
            values.record(&mut *self.fields.lock().unwrap());
        }
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn event(&self, _event: &Event<'_>) {}
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }
}

#[test]
fn first_use_span_records_freshness_once_per_origin() {
    let url = "https://first-use-span.test/v1";
    let origin = endpoint_origin(url).expect("dialable origin");
    warm_state().insert(origin.clone(), WarmState::Warmed(Instant::now()));

    let capture = std::sync::Arc::new(span_capture::Capture::new(
        "sampler.transport_prewarm_first_use",
    ));
    tracing::subscriber::with_default(capture.clone(), || {
        note_first_sampling_use(url);
        note_first_sampling_use(url);
    });

    let fields = capture.fields.lock().unwrap();
    assert_eq!(
        fields.span_count, 1,
        "only the first request per origin may emit the first-use span"
    );
    assert_eq!(
        fields.strs.get("endpoint").map(String::as_str),
        Some(origin.as_str()),
        "endpoint field must land on the first-use span"
    );
    assert_eq!(
        fields.strs.get("freshness").map(String::as_str),
        Some("warm_fresh"),
        "a warm younger than the idle window must read warm_fresh"
    );
    assert!(
        fields.ints.contains_key("age_at_first_use_ms"),
        "age_at_first_use_ms must land as an i64 the exporter keeps"
    );
}
