use std::time::Duration;

use super::*;
use crate::otel_fixtures::{logs_body, logs_event, metrics_body, metrics_event};

#[test]
fn body_text_is_every_kept_body_in_order() {
    let recorder = OtelRecorder::new();
    let first = logs_body("grok_code.session_start");
    let second = logs_body("grok_code.user_prompt");

    recorder.record_protobuf(OtelSignal::Logs, &first);
    recorder.record_protobuf(OtelSignal::Logs, &second);

    assert_eq!(
        Ok(String::from_utf8_lossy(&[first, second].concat()).into_owned()),
        recorder.body_text()
    );
}

#[test]
fn record_protobuf_logs_an_export_with_no_headers() {
    let recorder = OtelRecorder::new();
    let body = logs_body("grok_code.session_start");

    recorder.record_protobuf(OtelSignal::Logs, &body);

    assert_eq!(
        vec![OtelExport {
            signal: OtelSignal::Logs,
            headers: Vec::new(),
            body: OtelBody::Kept(body),
        }],
        recorder.exports()
    );
}

#[test]
fn body_past_the_retention_cap_keeps_only_its_length_and_fails_body_text() {
    let recorder = OtelRecorder::new();
    let past_the_cap = logs_body(&"x".repeat(MAX_RETAINED_BODY_BYTES));

    recorder.record_protobuf(OtelSignal::Logs, &past_the_cap);

    assert_eq!(
        (
            vec![OtelBody::PastRetentionCap {
                len: past_the_cap.len()
            }],
            Err(OtelRecorderError::BodyPastRetentionCap {
                signal: OtelSignal::Logs,
                len: past_the_cap.len(),
            }),
            1,
            Vec::new(),
        ),
        (
            recorder
                .exports()
                .into_iter()
                .map(|export| export.body)
                .collect::<Vec<_>>(),
            recorder.body_text(),
            recorder.events().len(),
            recorder.faults(),
        )
    );
}

#[test]
fn body_text_fails_once_the_export_log_evicted_an_entry() {
    let recorder = OtelRecorder::new();
    let body = logs_body("grok_code.user_prompt");

    for _ in 0..=MAX_LOGGED_EXPORTS {
        recorder.record_protobuf(OtelSignal::Logs, &body);
    }

    assert_eq!(
        Err(OtelRecorderError::ExportsEvicted { evicted: 1 }),
        recorder.body_text()
    );
}

#[test]
fn any_fault_fails_body_text_even_with_every_body_kept() {
    let recorder = OtelRecorder::new();
    let unserved = OtelFault::Unserved {
        method: "GET".to_owned(),
        path: "/v1/logs".to_owned(),
    };

    recorder.record_protobuf(OtelSignal::Logs, &logs_body("grok_code.session_start"));
    recorder.record_fault(unserved.clone());

    assert_eq!(
        Err(OtelRecorderError::Fault(unserved)),
        recorder.body_text()
    );
}

#[tokio::test(start_paused = true)]
async fn wait_for_signals_needs_every_signal_and_resolves_with_every_event() {
    let recorder = OtelRecorder::new();
    recorder.record_protobuf(OtelSignal::Logs, &logs_body("grok_code.session_start"));

    let (events, ()) = tokio::join!(
        recorder.wait_for_signals(
            Duration::from_secs(5),
            &[OtelSignal::Logs, OtelSignal::Metrics]
        ),
        async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            recorder.record_protobuf(OtelSignal::Metrics, &metrics_body("grok_code.token.usage"));
        },
    );

    assert_eq!(
        Ok(vec![
            logs_event("grok_code.session_start"),
            metrics_event("grok_code.token.usage"),
        ]),
        events
    );
}

#[tokio::test(start_paused = true)]
async fn wait_for_events_times_out_reporting_the_distinct_event_names_recorded() {
    let recorder = OtelRecorder::new();
    for name in [
        "grok_code.session_start",
        "grok_code.user_prompt",
        "grok_code.session_start",
    ] {
        recorder.record_protobuf(OtelSignal::Logs, &logs_body(name));
    }

    let outcome = recorder
        .wait_for_events(Duration::from_secs(5), |_| false)
        .await;

    assert_eq!(
        Err(OtelRecorderError::Timeout {
            timeout: crate::scaled(Duration::from_secs(5)),
            events_recorded: 3,
            event_names: vec![
                "grok_code.session_start".to_owned(),
                "grok_code.user_prompt".to_owned()
            ],
        }),
        outcome
    );
}

#[tokio::test(start_paused = true)]
async fn wait_for_silence_sees_only_events_recorded_after_the_call() {
    let recorder = OtelRecorder::new();
    let window = Duration::from_secs(5);
    recorder.record_protobuf(OtelSignal::Logs, &logs_body("grok_code.session_start"));

    let held = recorder
        .wait_for_silence(window, |events| !events.is_empty())
        .await;
    let (broken, ()) = tokio::join!(
        recorder.wait_for_silence(window, |events| !events.is_empty()),
        async { recorder.record_protobuf(OtelSignal::Logs, &logs_body("grok_code.user_prompt")) },
    );

    assert_eq!(
        (
            Ok(()),
            Err(OtelRecorderError::SilenceBroken {
                window,
                events: vec![logs_event("grok_code.user_prompt")],
            })
        ),
        (held, broken)
    );
}
