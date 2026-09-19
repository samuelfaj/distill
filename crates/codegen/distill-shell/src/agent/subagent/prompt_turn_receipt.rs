// Modified for Distill by Samuel Fajreldines, 2026.
//! Receipt settlement for protected parent-message turns.
//!
//! A clean turn waits indefinitely for its receipt; only unclean or cancel paths arm the grace deadline.

use std::sync::Arc;

use futures::{FutureExt, stream::FuturesUnordered, stream::StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::session::{
    CancelOptions, CancelTrigger, SessionCommand, ShutdownKind, commands::PromptTurnResult,
};
use distill_tools::implementations::distill::task::types::SubagentResult;

pub(super) const ACTIVE_MESSAGE_RECEIPT_CAPACITY: usize = 64;
const CANCELLED_RECEIPT_SETTLEMENT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) struct PromptTurnReceipt {
    pub prompt_id: String,
    pub result: oneshot::Receiver<PromptTurnResult>,
    pub telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry,
}

pub(super) enum PromptTurnReceiptOutcome {
    Settled(Box<Result<PromptTurnResult, oneshot::error::RecvError>>),
    TimedOut,
}

pub(super) struct FinalPromptTurnReceipt {
    pub prompt_id: String,
    pub outcome: PromptTurnReceiptOutcome,
    pub telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PromptTurnReceiptDisposition {
    Completed,
    Cancelled,
    AdmissionUncertain,
    TimedOut,
}

pub(super) struct PromptTurnReceiptSettlement {
    pub final_receipt: Option<FinalPromptTurnReceipt>,
    pub disposition: PromptTurnReceiptDisposition,
}

pub(super) struct PromptTurnSettlementInput {
    pub result: SubagentResult,
    pub disposition: PromptTurnReceiptDisposition,
    pub final_receipt: Option<Result<PromptTurnResult, oneshot::error::RecvError>>,
    pub final_text: String,
    pub was_cancelled: bool,
}

pub(super) struct PromptTurnSettlementOutput {
    pub result: SubagentResult,
    pub cancellation_may_hide_usage: bool,
    pub settlement_status: crate::session::telemetry::ActiveAgentMessageSettlementStatus,
}

pub(super) fn reduce_prompt_turn_settlement(
    input: PromptTurnSettlementInput,
) -> PromptTurnSettlementOutput {
    let PromptTurnSettlementInput {
        mut result,
        disposition,
        final_receipt,
        final_text,
        was_cancelled,
    } = input;

    if let PromptTurnReceiptDisposition::Completed = disposition {
        let is_final_receipt_closed = final_receipt.as_ref().is_some_and(Result::is_err);
        let cancellation_may_hide_usage = if let Some(receipt) = final_receipt {
            let empty_summary = String::new;
            let max_turns_summary = |_| String::new();
            let folded = super::prompt_turn_result::reduce_prompt_turn_result(
                super::prompt_turn_result::PromptTurnResultInput {
                    result,
                    turn_result: receipt,
                    mode: super::prompt_turn_result::PromptTurnResultMode::ParentFollowup,
                    final_text,
                    was_cancelled,
                    summaries: super::prompt_turn_result::PromptTurnResultSummaries {
                        success: &empty_summary,
                        max_turns: &max_turns_summary,
                        cancelled: &empty_summary,
                    },
                    result_tokens: 0,
                },
            );
            result = folded.result;
            folded.cancellation_may_hide_usage
        } else {
            false
        };
        return PromptTurnSettlementOutput {
            settlement_status: crate::session::telemetry::classify_completed_settlement(
                crate::session::telemetry::ActiveAgentMessageCompletedSettlement {
                    is_result_success: result.success,
                    is_result_cancelled: result.cancelled,
                    is_final_receipt_closed,
                },
            ),
            result,
            cancellation_may_hide_usage,
        };
    }

    let error = match disposition {
        PromptTurnReceiptDisposition::Cancelled => "Subagent was cancelled",
        PromptTurnReceiptDisposition::AdmissionUncertain => {
            "Active-message admission could not be proven settled"
        }
        PromptTurnReceiptDisposition::TimedOut => "Subagent receipt settlement timed out",
        PromptTurnReceiptDisposition::Completed => {
            unreachable!("completed settlements return before unclean error mapping")
        }
    };

    result.success = false;
    result.cancelled = true;
    result.error = Some(error.to_string());
    result.output = Arc::from(final_text);
    result.output_usage_incomplete = true;
    PromptTurnSettlementOutput {
        settlement_status: match disposition {
            PromptTurnReceiptDisposition::Cancelled => {
                crate::session::telemetry::ActiveAgentMessageSettlementStatus::Cancelled
            }
            PromptTurnReceiptDisposition::AdmissionUncertain => {
                crate::session::telemetry::ActiveAgentMessageSettlementStatus::AdmissionUncertain
            }
            PromptTurnReceiptDisposition::TimedOut => {
                crate::session::telemetry::ActiveAgentMessageSettlementStatus::TimedOut
            }
            PromptTurnReceiptDisposition::Completed => {
                unreachable!("completed settlements return before unclean error mapping")
            }
        },
        result,
        cancellation_may_hide_usage: true,
    }
}

#[derive(Clone, Copy)]
pub(super) enum AdmissionSettlement {
    Settled,
    Uncertain,
}

pub(super) struct PromptTurnReceiptDrain {
    finalization: Option<oneshot::Sender<AdmissionSettlement>>,
    settlement: oneshot::Receiver<PromptTurnReceiptSettlement>,
    _task: tokio_util::task::AbortOnDropHandle<()>,
}

impl PromptTurnReceiptDrain {
    pub(super) fn start(
        receipt_stream: mpsc::Receiver<PromptTurnReceipt>,
        child_cmd_tx: mpsc::UnboundedSender<SessionCommand>,
        cancel_token: CancellationToken,
    ) -> Self {
        let (finalization, finalization_rx) = oneshot::channel();
        let (settlement_tx, settlement) = oneshot::channel();
        let task = tokio::spawn(async move {
            let settlement = drain_prompt_turn_receipts(
                receipt_stream,
                &child_cmd_tx,
                cancel_token,
                finalization_rx,
            )
            .await;
            let _ = settlement_tx.send(settlement);
        });
        Self {
            finalization: Some(finalization),
            settlement,
            _task: tokio_util::task::AbortOnDropHandle::new(task),
        }
    }

    pub(super) async fn settle(
        mut self,
        admission: AdmissionSettlement,
    ) -> PromptTurnReceiptSettlement {
        let Some(finalization) = self.finalization.take() else {
            return uncertain_settlement();
        };
        if finalization.send(admission).is_err() {
            return uncertain_settlement();
        }
        (&mut self.settlement)
            .await
            .unwrap_or_else(|_| uncertain_settlement())
    }
}

pub(super) fn cancel_shell_child_turn(cmd_tx: &mpsc::UnboundedSender<SessionCommand>) {
    let _ = cmd_tx.send(SessionCommand::Cancel(CancelOptions {
        cancel_subagents: true,
        kill_background_tasks: true,
        trigger: Some(CancelTrigger::Shutdown),
        ..Default::default()
    }));
}

async fn drain_prompt_turn_receipts(
    mut receipt_stream: mpsc::Receiver<PromptTurnReceipt>,
    child_cmd_tx: &mpsc::UnboundedSender<SessionCommand>,
    cancel_token: CancellationToken,
    mut finalization: oneshot::Receiver<AdmissionSettlement>,
) -> PromptTurnReceiptSettlement {
    let mut pending = FuturesUnordered::new();
    let mut is_stream_open = true;
    let mut disposition = None;
    let mut deadline = None;
    let mut final_receipt = None;
    let mut final_identity = None;
    let mut latest_admission = 0u64;
    let mut latest_settlement = 0u64;

    loop {
        if disposition.is_some() && !is_stream_open && pending.is_empty() {
            return PromptTurnReceiptSettlement {
                final_receipt,
                disposition: disposition
                    .unwrap_or(PromptTurnReceiptDisposition::AdmissionUncertain),
            };
        }

        tokio::select! {
            biased;
            admission = &mut finalization, if disposition.is_none() => {
                receipt_stream.close();
                let next_disposition = match admission.unwrap_or(AdmissionSettlement::Uncertain) {
                    AdmissionSettlement::Settled if !cancel_token.is_cancelled() => {
                        PromptTurnReceiptDisposition::Completed
                    }
                    AdmissionSettlement::Settled => PromptTurnReceiptDisposition::Cancelled,
                    AdmissionSettlement::Uncertain => {
                        PromptTurnReceiptDisposition::AdmissionUncertain
                    }
                };
                // WHY: Completed deliberately arms no deadline (admitted work may run long); termination relies on the receipt sender always being dropped or sent when the turn ends.
                if next_disposition != PromptTurnReceiptDisposition::Completed {
                    cancel_shell_child_turn(child_cmd_tx);
                    deadline = Some(
                        tokio::time::Instant::now() + CANCELLED_RECEIPT_SETTLEMENT_GRACE,
                    );
                }
                disposition = Some(next_disposition);
            }
            _ = cancel_token.cancelled(), if disposition == Some(PromptTurnReceiptDisposition::Completed) => {
                disposition = Some(PromptTurnReceiptDisposition::Cancelled);
                cancel_shell_child_turn(child_cmd_tx);
                deadline = Some(
                    tokio::time::Instant::now() + CANCELLED_RECEIPT_SETTLEMENT_GRACE,
                );
            }
            receipt = receipt_stream.recv(), if is_stream_open => {
                match receipt {
                    Some(receipt) => {
                        latest_admission = latest_admission.saturating_add(1);
                        final_identity = Some((
                            receipt.prompt_id.clone(),
                            receipt.telemetry.clone(),
                        ));
                        pending.push(async move {
                            let settled = FinalPromptTurnReceipt {
                                prompt_id: receipt.prompt_id,
                                outcome: PromptTurnReceiptOutcome::Settled(Box::new(
                                    receipt.result.await,
                                )),
                                telemetry: receipt.telemetry,
                            };
                            (latest_admission, settled)
                        });
                    }
                    None => is_stream_open = false,
                }
            }
            Some((admission, settled)) = pending.next(), if !pending.is_empty() => {
                if admission > latest_settlement {
                    latest_settlement = admission;
                    final_receipt = Some(settled);
                }
            }
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            }, if deadline.is_some() => {
                while let Ok(receipt) = receipt_stream.try_recv() {
                    final_identity = Some((receipt.prompt_id, receipt.telemetry));
                }
                // Ready pending identities were recorded at ingress; completion order is not admission order.
                while pending.next().now_or_never().flatten().is_some() {}
                return force_receipt_settlement_timeout(child_cmd_tx, final_identity);
            }
        }
    }
}

fn force_receipt_settlement_timeout(
    child_cmd_tx: &mpsc::UnboundedSender<SessionCommand>,
    final_identity: Option<(
        String,
        crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry,
    )>,
) -> PromptTurnReceiptSettlement {
    let _ = child_cmd_tx.send(SessionCommand::Shutdown(ShutdownKind::CancelRunningTurn));
    PromptTurnReceiptSettlement {
        final_receipt: final_identity.map(|(prompt_id, telemetry)| FinalPromptTurnReceipt {
            prompt_id,
            outcome: PromptTurnReceiptOutcome::TimedOut,
            telemetry,
        }),
        disposition: PromptTurnReceiptDisposition::TimedOut,
    }
}

fn uncertain_settlement() -> PromptTurnReceiptSettlement {
    PromptTurnReceiptSettlement {
        final_receipt: None,
        disposition: PromptTurnReceiptDisposition::AdmissionUncertain,
    }
}

#[cfg(test)]
#[path = "prompt_turn_receipt_tests.rs"]
mod tests;
