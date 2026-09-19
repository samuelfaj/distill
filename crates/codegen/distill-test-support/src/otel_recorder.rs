//! The typed log behind the mock OTLP server, in arrival order. Public so a test with a transport of its
//! own (gRPC, TLS) records into it and reads it through the same readers.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::bounded_log::BoundedLog;
use crate::otel_decode;
use crate::otel_event::{
    OtelBody, OtelDecodeError, OtelEvent, OtelExport, OtelFault, OtelLogRecord, OtelMetricPoint,
    OtelSignal, OtelUnreadBody,
};
use crate::watched::{WaitOutcome, Watched};

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum OtelRecorderError {
    /// `timeout` is the budget the wait ran for, after `GROK_TEST_TIMEOUT_SCALE`.
    #[error(
        "no matching OpenTelemetry event within {timeout:?}; {events_recorded} events recorded, log record names {event_names:?}"
    )]
    Timeout {
        timeout: Duration,
        events_recorded: usize,
        event_names: Vec<String>,
    },
    /// `events` are the ones recorded inside the window, not everything the log held.
    #[error(
        "{} OpenTelemetry events recorded within a {window:?} window that had to stay silent",
        .events.len()
    )]
    SilenceBroken {
        window: Duration,
        events: Vec<OtelEvent>,
    },
    #[error(
        "an OpenTelemetry {signal:?} body of {len} bytes was past the retention cap, so the body text is incomplete"
    )]
    BodyPastRetentionCap { signal: OtelSignal, len: usize },
    #[error(
        "{evicted} OpenTelemetry exports were evicted from the log, so the body text is incomplete"
    )]
    ExportsEvicted { evicted: usize },
    /// The first fault recorded, however many followed.
    #[error(transparent)]
    Fault(OtelFault),
}

/// Caps sized for one long session; each log evicts oldest first past its cap. Faults are not
/// capped: they are rare, and the first one must stay the first.
const MAX_LOGGED_EVENTS: usize = 16_384;
const MAX_LOGGED_EXPORTS: usize = 1_024;

/// A larger body is kept only by its length, so the export log's memory is bounded.
const MAX_RETAINED_BODY_BYTES: usize = 256 * 1024;

struct RecorderLog {
    events: BoundedLog<OtelEvent>,
    exports: BoundedLog<OtelExport>,
    faults: Vec<OtelFault>,
}

/// What arrived of one export body. [`OtelRecorder::record`] logs the export and its fault in one
/// step, so an unread body is never logged without the fault that says why.
pub(crate) enum ReceivedBody<'a> {
    InFull {
        bytes: &'a [u8],
        decoded: Result<Vec<OtelEvent>, OtelDecodeError>,
    },
    Unread {
        body: OtelUnreadBody,
        error: String,
    },
}

#[derive(Clone)]
pub struct OtelRecorder {
    log: Arc<Watched<RecorderLog>>,
}

impl Default for OtelRecorder {
    fn default() -> OtelRecorder {
        OtelRecorder::new()
    }
}

impl OtelRecorder {
    #[must_use]
    pub fn new() -> OtelRecorder {
        OtelRecorder {
            log: Arc::new(Watched::new(RecorderLog {
                events: BoundedLog::new(MAX_LOGGED_EVENTS),
                exports: BoundedLog::new(MAX_LOGGED_EXPORTS),
                faults: Vec::new(),
            })),
        }
    }

    /// For a transport of the test's own; the export has no headers.
    pub fn record_protobuf(&self, signal: OtelSignal, message: &[u8]) {
        let received = ReceivedBody::InFull {
            bytes: message,
            decoded: otel_decode::decode_protobuf(signal, message),
        };
        self.record(signal, Vec::new(), received);
    }

    pub(crate) fn record(
        &self,
        signal: OtelSignal,
        headers: Vec<(String, String)>,
        received: ReceivedBody<'_>,
    ) {
        let (body, decoded) = match received {
            ReceivedBody::InFull { bytes, decoded } => (
                if bytes.len() <= MAX_RETAINED_BODY_BYTES {
                    OtelBody::Kept(bytes.to_vec())
                } else {
                    OtelBody::PastRetentionCap { len: bytes.len() }
                },
                decoded.map_err(|error| OtelFault::Undecodable {
                    signal,
                    len: bytes.len(),
                    error,
                }),
            ),
            ReceivedBody::Unread { body, error } => (
                OtelBody::Unread(body),
                Err(OtelFault::Unread {
                    signal,
                    body,
                    error,
                }),
            ),
        };
        self.log.update(|log| {
            match decoded {
                Ok(events) => log.events.extend(events),
                Err(fault) => log.faults.push(fault),
            }
            log.exports.push(OtelExport {
                signal,
                headers,
                body,
            });
        });
    }

    pub(crate) fn record_fault(&self, fault: OtelFault) {
        self.log.update(|log| log.faults.push(fault));
    }

    pub fn events(&self) -> Vec<OtelEvent> {
        self.log.read(|log| log.events.to_vec())
    }

    pub fn log_records(&self) -> Vec<OtelLogRecord> {
        self.events()
            .into_iter()
            .filter_map(OtelEvent::into_log_record)
            .collect()
    }

    pub fn metric_points(&self) -> Vec<OtelMetricPoint> {
        self.events()
            .into_iter()
            .filter_map(OtelEvent::into_metric_point)
            .collect()
    }

    pub fn log_record(&self, event_name: &str) -> Option<OtelLogRecord> {
        self.log_records()
            .into_iter()
            .find(|record| record.event_name == event_name)
    }

    pub fn metric_points_named(&self, name: &str) -> Vec<OtelMetricPoint> {
        self.metric_points()
            .into_iter()
            .filter(|point| point.name == name)
            .collect()
    }

    /// Distinct log record event names in order of first appearance.
    pub fn event_names(&self) -> Vec<String> {
        distinct_event_names(self.events())
    }

    pub fn exports(&self) -> Vec<OtelExport> {
        self.log.read(|log| log.exports.to_vec())
    }

    pub fn faults(&self) -> Vec<OtelFault> {
        self.log.read(|log| log.faults.clone())
    }

    /// Every export body as lossy text, for a scan that no secret or canary reached the wire. A
    /// fault, an evicted export, or a body past the retention cap is bytes the scan would miss.
    pub fn body_text(&self) -> Result<String, OtelRecorderError> {
        let bytes = self.log.read(|log| {
            if let Some(fault) = log.faults.first() {
                return Err(OtelRecorderError::Fault(fault.clone()));
            }
            let evicted = log.exports.evicted();
            if evicted > 0 {
                return Err(OtelRecorderError::ExportsEvicted { evicted });
            }
            log.exports
                .iter()
                .map(|export| match &export.body {
                    OtelBody::Kept(bytes) => Ok(bytes.as_slice()),
                    OtelBody::PastRetentionCap { len } => {
                        Err(OtelRecorderError::BodyPastRetentionCap {
                            signal: export.signal,
                            len: *len,
                        })
                    }
                    OtelBody::Unread(_) => {
                        unreachable!("an unread body is logged with its fault, returned above")
                    }
                })
                .collect::<Result<Vec<&[u8]>, OtelRecorderError>>()
                .map(|bodies| bodies.concat())
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Resolves with every event recorded so far, not only the ones `is_satisfied` matched.
    /// `is_satisfied` sees a snapshot, so it may read the recorder. `timeout` is scaled by
    /// `GROK_TEST_TIMEOUT_SCALE` like every harness budget.
    pub async fn wait_for_events(
        &self,
        timeout: Duration,
        mut is_satisfied: impl FnMut(&[OtelEvent]) -> bool,
    ) -> Result<Vec<OtelEvent>, OtelRecorderError> {
        let timeout = crate::scaled(timeout);
        let deadline = Instant::now() + timeout;
        let outcome = self
            .wait_unless_faulted(
                deadline,
                |log| log.events.to_vec(),
                |events| is_satisfied(events).then(|| events.clone()),
            )
            .await?;
        match outcome {
            WaitOutcome::Accepted(events) => Ok(events),
            WaitOutcome::DeadlinePassed(events) => Err(OtelRecorderError::Timeout {
                timeout,
                events_recorded: events.len(),
                event_names: distinct_event_names(events),
            }),
        }
    }

    /// Resolves once at least one event of every signal in `signals` is recorded.
    pub async fn wait_for_signals(
        &self,
        timeout: Duration,
        signals: &[OtelSignal],
    ) -> Result<Vec<OtelEvent>, OtelRecorderError> {
        self.wait_for_events(timeout, |events| {
            signals
                .iter()
                .all(|signal| events.iter().any(|event| event.signal() == *signal))
        })
        .await
    }

    /// `is_broken` sees only the events recorded since the call. `window` is not scaled: a
    /// silence window is a claim about the exporter, not a budget.
    pub async fn wait_for_silence(
        &self,
        window: Duration,
        mut is_broken: impl FnMut(&[OtelEvent]) -> bool,
    ) -> Result<(), OtelRecorderError> {
        let baseline = self.log.read(|log| log.events.total());
        let deadline = Instant::now() + window;
        let outcome = self
            .wait_unless_faulted(
                deadline,
                |log| log.events.arrived_since(baseline),
                |fresh| is_broken(fresh).then(|| fresh.clone()),
            )
            .await?;
        match outcome {
            WaitOutcome::Accepted(events) => {
                Err(OtelRecorderError::SilenceBroken { window, events })
            }
            WaitOutcome::DeadlinePassed(_) => Ok(()),
        }
    }

    /// A fault ends the wait as an error the moment it is logged, whether or not `accept` would
    /// take what `view` saw, so no wait passes over a body the server could not record.
    async fn wait_unless_faulted<S, R>(
        &self,
        deadline: Instant,
        view: impl Fn(&RecorderLog) -> S,
        mut accept: impl FnMut(&S) -> Option<R>,
    ) -> Result<WaitOutcome<R, S>, OtelRecorderError> {
        let snapshot = |log: &RecorderLog| Snapshot {
            first_fault: log.faults.first().cloned(),
            seen: view(log),
        };
        let probe = |snapshot: &Snapshot<S>| match &snapshot.first_fault {
            Some(fault) => Some(Probe::Faulted(fault.clone())),
            None => accept(&snapshot.seen).map(Probe::Satisfied),
        };
        match self.log.wait_until(deadline, snapshot, probe).await {
            WaitOutcome::Accepted(Probe::Faulted(fault)) => Err(OtelRecorderError::Fault(fault)),
            WaitOutcome::Accepted(Probe::Satisfied(found)) => Ok(WaitOutcome::Accepted(found)),
            WaitOutcome::DeadlinePassed(snapshot) => Ok(WaitOutcome::DeadlinePassed(snapshot.seen)),
        }
    }
}

struct Snapshot<S> {
    first_fault: Option<OtelFault>,
    seen: S,
}

enum Probe<R> {
    Faulted(OtelFault),
    Satisfied(R),
}

fn distinct_event_names(events: Vec<OtelEvent>) -> Vec<String> {
    let mut seen = HashSet::new();
    events
        .into_iter()
        .filter_map(OtelEvent::into_log_record)
        .filter_map(|record| {
            seen.insert(record.event_name.clone())
                .then_some(record.event_name)
        })
        .collect()
}

#[cfg(test)]
#[path = "otel_recorder_tests.rs"]
mod tests;
