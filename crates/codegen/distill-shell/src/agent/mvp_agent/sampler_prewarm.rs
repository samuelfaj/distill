// Modified for Distill by Samuel Fajreldines, 2026.
//! Spawns the sampling-transport prewarm at session create/resume and records
//! its outcome on a detached `session.sampler_transport_prewarm` span.

use super::*;

impl MvpAgent {
    /// Prewarms the resident model's base URL when the final resolution
    /// differs from the one already dialed at spawn.
    pub(super) fn prewarm_final_model_base_url(
        &self,
        session_id: &acp::SessionId,
        provisional_base_url: &str,
        origin_client: Option<crate::http::OriginClientInfo>,
    ) {
        let Some(final_model_id) = self.resident_handle(session_id).map(|h| h.model_id.clone())
        else {
            return;
        };
        let final_base_url = self
            .resolve_sampling_config_for_model(&final_model_id, origin_client)
            .base_url;
        if let Some(base_url) = prewarm_base_url_if_changed(provisional_base_url, &final_base_url) {
            spawn_sampler_transport_prewarm(&base_url);
        }
    }
}

// Detached by design: the warm state is process-global, not session state,
// and the dial is PREWARM_TIMEOUT-bounded.
pub(super) fn spawn_sampler_transport_prewarm(base_url: &str) {
    tokio::spawn(prewarm_and_record(base_url.to_owned()));
}

fn prewarm_base_url_if_changed(already_warmed: &str, resolved: &str) -> Option<String> {
    (already_warmed != resolved).then(|| resolved.to_owned())
}

struct PrewarmCancelGuard {
    span: tracing::Span,
    started: std::time::Instant,
    completed: bool,
}

impl Drop for PrewarmCancelGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.span.record("outcome", "cancelled");
            let duration_ms = i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX);
            self.span.record("duration_ms", duration_ms);
        }
    }
}

fn prewarm_and_record(base_url: String) -> impl std::future::Future<Output = ()> {
    use tracing::Instrument;
    let span = sampler_prewarm_span();
    let record_span = span.clone();
    async move {
        let mut guard = PrewarmCancelGuard {
            span: record_span.clone(),
            started: std::time::Instant::now(),
            completed: false,
        };
        let report = distill_sampler::prewarm_transport(&base_url).await;
        record_prewarm_report(&record_span, &report);
        guard.completed = true;
    }
    .instrument(span)
}

fn sampler_prewarm_span() -> tracing::Span {
    tracing::info_span!(
        parent: None,
        "session.sampler_transport_prewarm",
        outcome = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
        endpoint = tracing::field::Empty,
    )
}

fn record_prewarm_report(span: &tracing::Span, report: &distill_sampler::PrewarmReport) {
    span.record("outcome", <&'static str>::from(report.outcome));
    span.record("duration_ms", report.duration_ms);
    if let Some(origin) = &report.origin {
        span.record("endpoint", origin.as_str());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::util::SubscriberInitExt;

    use super::{
        PrewarmCancelGuard, prewarm_and_record, record_prewarm_report, sampler_prewarm_span,
    };

    #[derive(Clone, Default)]
    struct Captured {
        name: String,
        strs: BTreeMap<String, String>,
        ints: BTreeMap<String, i64>,
    }

    #[derive(Default)]
    struct Visitor {
        strs: BTreeMap<String, String>,
        ints: BTreeMap<String, i64>,
    }

    impl Visit for Visitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.strs
                .insert(field.name().to_string(), value.to_string());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.ints.insert(field.name().to_string(), value);
        }
        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    struct Collector {
        spans: Arc<Mutex<HashMap<u64, Captured>>>,
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Collector {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
            let mut v = Visitor::default();
            attrs.record(&mut v);
            self.spans.lock().unwrap().insert(
                id.into_u64(),
                Captured {
                    name: attrs.metadata().name().to_string(),
                    strs: v.strs,
                    ints: v.ints,
                },
            );
        }

        fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
            let mut v = Visitor::default();
            values.record(&mut v);
            let mut map = self.spans.lock().unwrap();
            let entry = map.entry(id.into_u64()).or_default();
            entry.strs.extend(v.strs);
            entry.ints.extend(v.ints);
        }
    }

    fn collect_spans() -> (
        Arc<Mutex<HashMap<u64, Captured>>>,
        tracing::subscriber::DefaultGuard,
    ) {
        let spans = Arc::new(Mutex::new(HashMap::new()));
        let guard = tracing_subscriber::registry()
            .with(Collector {
                spans: spans.clone(),
            })
            .set_default();
        (spans, guard)
    }

    fn recorded_prewarm_span(spans: &Mutex<HashMap<u64, Captured>>, phase: &str) -> Captured {
        spans
            .lock()
            .unwrap()
            .values()
            .find(|c| c.name == "session.sampler_transport_prewarm")
            .cloned()
            .unwrap_or_else(|| panic!("{phase}: prewarm span was created under the subscriber"))
    }

    #[tokio::test]
    async fn prewarm_telemetry() {
        let (spans, guard) = collect_spans();
        let span = sampler_prewarm_span();
        record_prewarm_report(
            &span,
            &distill_sampler::PrewarmReport {
                outcome: distill_sampler::PrewarmOutcome::Warmed,
                duration_ms: 7,
                origin: Some("https://api.example.test".to_string()),
            },
        );
        drop(guard);
        let captured = recorded_prewarm_span(&spans, "report phase");
        assert_eq!(
            captured.strs.get("outcome").map(String::as_str),
            Some("warmed"),
            "report phase: outcome must land on the span"
        );
        assert_eq!(
            captured.ints.get("duration_ms").copied(),
            Some(7),
            "report phase: duration_ms must land on the span"
        );
        assert_eq!(
            captured.strs.get("endpoint").map(String::as_str),
            Some("https://api.example.test"),
            "report phase: endpoint (origin) must land on the span so outcomes are sliceable by origin"
        );

        let (spans, guard) = collect_spans();
        prewarm_and_record("not a url".to_string()).await;
        drop(guard);
        let captured = recorded_prewarm_span(&spans, "future phase");
        assert_eq!(
            captured.strs.get("outcome").map(String::as_str),
            Some("no_origin"),
            "future phase: outcome recorded from the spawned block onto its span handle"
        );
        assert!(
            captured.ints.contains_key("duration_ms"),
            "future phase: duration_ms recorded from the spawned block"
        );

        let (spans, guard) = collect_spans();
        let span = sampler_prewarm_span();
        drop(PrewarmCancelGuard {
            span,
            started: std::time::Instant::now(),
            completed: false,
        });
        drop(guard);
        let captured = recorded_prewarm_span(&spans, "cancel phase");
        assert_eq!(
            captured.strs.get("outcome").map(String::as_str),
            Some("cancelled"),
            "cancel phase: a cancelled prewarm must record a cancelled outcome"
        );
        assert!(
            captured.ints.contains_key("duration_ms"),
            "cancel phase: a cancelled prewarm must record a duration_ms"
        );
    }

    #[test]
    fn prewarm_base_url_if_changed_warms_resolved_unless_already_warmed() {
        use super::prewarm_base_url_if_changed;
        assert_eq!(
            prewarm_base_url_if_changed("https://provisional", "https://resolved"),
            Some("https://resolved".to_string())
        );
        assert_eq!(
            prewarm_base_url_if_changed("https://same", "https://same"),
            None
        );
    }
}
