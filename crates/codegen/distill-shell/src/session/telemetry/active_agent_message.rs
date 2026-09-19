// Modified for Distill by Samuel Fajreldines, 2026.
//! Turns active-agent message tool outputs and settlements into telemetry events and emits them.

use std::sync::Arc;
use std::time::Instant;

use distill_telemetry::TelemetryCtx;
use distill_telemetry::events::{
    ActiveAgentMessageCompleted as Completed,
    ActiveAgentMessageFallbackDisposition as FallbackDisposition,
    ActiveAgentMessageFallbackReason as FallbackReason, ActiveAgentMessageLimitHit as LimitHit,
    ActiveAgentMessageOperation as Operation, ActiveAgentMessageOutcome as Outcome,
    ActiveAgentMessageQuotaHit as QuotaHit, ActiveAgentMessageQuotaKind as QuotaKind,
    ActiveAgentMessageSafePointTrigger as SafePointTrigger, ActiveAgentMessageSettled as Settled,
    ActiveAgentMessageSettlementDisposition as SettlementDisposition,
};
use distill_tools::implementations::distill::send_subagent_message::{
    SendSubagentMessageInput, SendSubagentMessageOutput,
};
use distill_tools::implementations::distill::task::types::ActiveAgentMessageOperation;
use distill_tools::types::output::ToolOutput;
use serde::Deserialize;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ActiveAgentMessageEvent {
    Completed(Completed),
    LimitHit(LimitHit),
    QuotaHit(QuotaHit),
    Settled(Settled),
}

/// Recorded once, at the first safe-point delivery of the message.
#[derive(Clone, Copy)]
struct SafePointDelivery {
    latency_ms: u64,
    trigger: SafePointTrigger,
}

#[derive(Default)]
struct ActiveAgentMessageDeliveryTelemetry {
    fallback_reason: parking_lot::Mutex<Option<FallbackReason>>,
    safe_point: parking_lot::Mutex<Option<SafePointDelivery>>,
}

#[derive(Clone)]
pub(crate) struct ActiveAgentMessageAdmissionTelemetry {
    pub admitted_at: Instant,
    pub parent_ctx: TelemetryCtx,
    requested_operation: Operation,
    effective_operation: Operation,
    delivery: Arc<ActiveAgentMessageDeliveryTelemetry>,
}

impl ActiveAgentMessageAdmissionTelemetry {
    pub(crate) fn new(
        admitted_at: Instant,
        parent_ctx: TelemetryCtx,
        requested_operation: ActiveAgentMessageOperation,
        effective_operation: ActiveAgentMessageOperation,
        fallback_reason: Option<FallbackReason>,
    ) -> Self {
        Self {
            admitted_at,
            parent_ctx,
            requested_operation: operation(requested_operation),
            effective_operation: operation(effective_operation),
            delivery: Arc::new(ActiveAgentMessageDeliveryTelemetry {
                fallback_reason: parking_lot::Mutex::new(fallback_reason),
                safe_point: parking_lot::Mutex::new(None),
            }),
        }
    }

    pub(crate) fn record_fallback(&self, reason: FallbackReason) {
        *self.delivery.fallback_reason.lock() = Some(reason);
    }

    pub(crate) fn record_safe_point_delivery(
        &self,
        delivered_at: Instant,
        trigger: SafePointTrigger,
    ) {
        let latency_ms = delivered_at
            .saturating_duration_since(self.admitted_at)
            .as_millis() as u64;
        self.delivery
            .safe_point
            .lock()
            .get_or_insert(SafePointDelivery {
                latency_ms,
                trigger,
            });
    }
}

pub(crate) use distill_telemetry::events::{
    ActiveAgentMessageFallbackReason, ActiveAgentMessageSafePointTrigger,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActiveAgentMessageSettlementStatus {
    Completed,
    Failed,
    Cancelled,
    ReceiptClosed,
    TimedOut,
    AdmissionUncertain,
}

pub(crate) struct ActiveAgentMessageCompletedSettlement {
    pub is_result_success: bool,
    pub is_result_cancelled: bool,
    pub is_final_receipt_closed: bool,
}

pub(crate) fn classify_completed_settlement(
    settlement: ActiveAgentMessageCompletedSettlement,
) -> ActiveAgentMessageSettlementStatus {
    if settlement.is_result_cancelled {
        ActiveAgentMessageSettlementStatus::Cancelled
    } else if settlement.is_final_receipt_closed {
        ActiveAgentMessageSettlementStatus::ReceiptClosed
    } else if settlement.is_result_success {
        ActiveAgentMessageSettlementStatus::Completed
    } else {
        ActiveAgentMessageSettlementStatus::Failed
    }
}

fn operation(operation: ActiveAgentMessageOperation) -> Operation {
    match operation {
        ActiveAgentMessageOperation::Queue => Operation::Queue,
        ActiveAgentMessageOperation::Steer => Operation::Steer,
        ActiveAgentMessageOperation::Interject => Operation::Interject,
    }
}

pub(crate) trait ActiveAgentMessageEventSink {
    fn emit(&mut self, event: ActiveAgentMessageEvent);
}

pub(crate) struct ProductEventSink;

impl ActiveAgentMessageEventSink for ProductEventSink {
    fn emit(&mut self, event: ActiveAgentMessageEvent) {
        match event {
            ActiveAgentMessageEvent::Completed(event) => {
                distill_telemetry::session_ctx::log_event(event);
            }
            ActiveAgentMessageEvent::LimitHit(event) => {
                distill_telemetry::session_ctx::log_event(event);
            }
            ActiveAgentMessageEvent::QuotaHit(event) => {
                distill_telemetry::session_ctx::log_event(event);
            }
            ActiveAgentMessageEvent::Settled(event) => {
                distill_telemetry::session_ctx::log_event(event);
            }
        }
    }
}

fn emit_immediate_events<S: ActiveAgentMessageEventSink + ?Sized>(
    output: &ToolOutput,
    requested_operation: ActiveAgentMessageOperation,
    duration_ms: u64,
    sink: &mut S,
) {
    let ToolOutput::SendSubagentMessage(output) = output else {
        return;
    };
    let mut quota_hit = None;
    let (outcome, limit_hit) = match output {
        SendSubagentMessageOutput::Accepted { .. } => (Outcome::Accepted, None),
        SendSubagentMessageOutput::NotFoundOrNotOwned => (Outcome::NotFoundOrNotOwned, None),
        SendSubagentMessageOutput::NotActiveOrFinalizing => (Outcome::NotActiveOrFinalizing, None),
        SendSubagentMessageOutput::Saturated { .. } => (Outcome::Saturated, None),
        SendSubagentMessageOutput::QuotaExceeded { kind, limit } => {
            let kind = match kind {
                distill_tools::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight => QuotaKind::SenderTargetInFlight,
                distill_tools::implementations::distill::task::types::ActiveAgentMessageQuotaKind::AttemptOutbound => QuotaKind::AttemptOutbound,
                _ => return,
            };
            quota_hit = Some(QuotaHit {
                kind,
                limit: u64::try_from(*limit).unwrap_or(u64::MAX),
            });
            (Outcome::QuotaExceeded, None)
        }
        SendSubagentMessageOutput::AdmissionUncertain => (Outcome::AdmissionUncertain, None),
        SendSubagentMessageOutput::NotAcceptedBeforeDeadline => {
            (Outcome::NotAcceptedBeforeDeadline, None)
        }
        SendSubagentMessageOutput::Unsupported => (Outcome::Unsupported, None),
        SendSubagentMessageOutput::Limit {
            max_bytes,
            observed_bytes,
        } if observed_bytes > max_bytes => (
            Outcome::Limit,
            Some(LimitHit {
                max_bytes: u64::try_from(*max_bytes).unwrap_or(u64::MAX),
                observed_bytes: u64::try_from(*observed_bytes).unwrap_or(u64::MAX),
            }),
        ),
        SendSubagentMessageOutput::Limit { .. } => (Outcome::Invalid, None),
        SendSubagentMessageOutput::ChannelClosed => (Outcome::ChannelClosed, None),
        _ => (Outcome::AdmissionUncertain, None),
    };
    sink.emit(ActiveAgentMessageEvent::Completed(Completed {
        outcome,
        requested_operation: operation(requested_operation),
        duration_ms,
    }));
    if let Some(limit_hit) = limit_hit {
        sink.emit(ActiveAgentMessageEvent::LimitHit(limit_hit));
    }
    if let Some(quota_hit) = quota_hit {
        sink.emit(ActiveAgentMessageEvent::QuotaHit(quota_hit));
    }
}

/// Telemetry only; the tool already parsed these arguments, so a failure here changes nothing.
fn requested_operation_from_args(parsed_args: &serde_json::Value) -> ActiveAgentMessageOperation {
    match SendSubagentMessageInput::deserialize(parsed_args) {
        Ok(input) => input.operation(),
        Err(error) => {
            tracing::debug!(
                %error,
                "active-message telemetry: tool arguments did not parse; recording steer"
            );
            ActiveAgentMessageOperation::Steer
        }
    }
}

pub(crate) fn record_completed_tool_output(
    output: &ToolOutput,
    parsed_args: &serde_json::Value,
    duration_ms: u64,
) {
    if !matches!(output, ToolOutput::SendSubagentMessage(_)) {
        return;
    }
    let requested_operation = requested_operation_from_args(parsed_args);
    #[cfg(test)]
    if try_emit_test_event(output, requested_operation, duration_ms) {
        return;
    }

    emit_immediate_events(
        output,
        requested_operation,
        duration_ms,
        &mut ProductEventSink,
    );
}

#[cfg(test)]
thread_local! {
    static TEST_EVENT_SINK: std::cell::RefCell<Option<Box<dyn ActiveAgentMessageEventSink>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn try_emit_test_event(
    output: &ToolOutput,
    requested_operation: ActiveAgentMessageOperation,
    duration_ms: u64,
) -> bool {
    TEST_EVENT_SINK.with_borrow_mut(|slot| {
        let Some(sink) = slot.as_mut() else {
            return false;
        };
        emit_immediate_events(output, requested_operation, duration_ms, sink.as_mut());
        true
    })
}

#[cfg(test)]
struct CapturedEventSink {
    events: std::rc::Rc<std::cell::RefCell<Vec<ActiveAgentMessageEvent>>>,
}

#[cfg(test)]
impl ActiveAgentMessageEventSink for CapturedEventSink {
    fn emit(&mut self, event: ActiveAgentMessageEvent) {
        self.events.borrow_mut().push(event);
    }
}

#[cfg(test)]
struct TestEventSinkGuard(Option<Box<dyn ActiveAgentMessageEventSink>>);

#[cfg(test)]
impl Drop for TestEventSinkGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        TEST_EVENT_SINK.with_borrow_mut(|slot| *slot = previous);
    }
}

#[cfg(test)]
pub(crate) async fn capture_product_events<F: std::future::Future>(
    future: F,
) -> (F::Output, Vec<ActiveAgentMessageEvent>) {
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let previous = TEST_EVENT_SINK.with_borrow_mut(|slot| {
        slot.replace(Box::new(CapturedEventSink {
            events: std::rc::Rc::clone(&events),
        }))
    });
    let guard = TestEventSinkGuard(previous);
    let output = future.await;
    drop(guard);
    let captured = {
        let mut events = events.borrow_mut();
        std::mem::take(&mut *events)
    };
    (output, captured)
}

#[cfg(test)]
fn record_completed_tool_output_with_sink(
    output: &ToolOutput,
    requested_operation: ActiveAgentMessageOperation,
    duration_ms: u64,
    sink: &mut impl ActiveAgentMessageEventSink,
) {
    emit_immediate_events(output, requested_operation, duration_ms, sink);
}

pub(crate) fn project_settlement(
    admission: Option<ActiveAgentMessageAdmissionTelemetry>,
    status: ActiveAgentMessageSettlementStatus,
    settled_at: Instant,
) -> Option<(TelemetryCtx, Settled)> {
    let admission = admission?;
    let disposition = match status {
        ActiveAgentMessageSettlementStatus::Completed => SettlementDisposition::Completed,
        ActiveAgentMessageSettlementStatus::Failed => SettlementDisposition::Failed,
        ActiveAgentMessageSettlementStatus::Cancelled => SettlementDisposition::Cancelled,
        ActiveAgentMessageSettlementStatus::ReceiptClosed => SettlementDisposition::ReceiptClosed,
        ActiveAgentMessageSettlementStatus::TimedOut => SettlementDisposition::TimedOut,
        ActiveAgentMessageSettlementStatus::AdmissionUncertain => {
            SettlementDisposition::AdmissionUncertain
        }
    };
    let fallback_reason = *admission.delivery.fallback_reason.lock();
    let safe_point = *admission.delivery.safe_point.lock();
    Some((
        admission.parent_ctx,
        Settled {
            disposition,
            requested_operation: admission.requested_operation,
            effective_operation: admission.effective_operation,
            fallback_disposition: if fallback_reason.is_some() {
                FallbackDisposition::Queued
            } else {
                FallbackDisposition::NotApplicable
            },
            fallback_reason,
            safe_point_latency_ms: safe_point.map(|delivery| delivery.latency_ms),
            safe_point_trigger: safe_point.map(|delivery| delivery.trigger),
            duration_ms: settled_at
                .saturating_duration_since(admission.admitted_at)
                .as_millis() as u64,
        },
    ))
}

pub(crate) async fn record_settlement(
    admission: Option<ActiveAgentMessageAdmissionTelemetry>,
    status: ActiveAgentMessageSettlementStatus,
) {
    let Some((parent_ctx, event)) = project_settlement(admission, status, Instant::now()) else {
        return;
    };
    distill_telemetry::with_session_ctx(parent_ctx, async {
        ProductEventSink.emit(ActiveAgentMessageEvent::Settled(event));
    })
    .await;
}

#[cfg(test)]
#[path = "active_agent_message_tests.rs"]
mod tests;
