// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn await_with_timeout<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("receipt test wait timed out")
}

fn receipt(prompt_id: &str) -> (oneshot::Sender<PromptTurnResult>, PromptTurnReceipt) {
    receipt_for_turn(prompt_id, 4)
}

fn receipt_for_turn(
    prompt_id: &str,
    turn: usize,
) -> (oneshot::Sender<PromptTurnResult>, PromptTurnReceipt) {
    let (result_tx, result) = oneshot::channel();
    (
        result_tx,
        PromptTurnReceipt {
            prompt_id: prompt_id.to_owned(),
            result,
            telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry::new(
                std::time::Instant::now(),
                distill_telemetry::TelemetryCtx::new(
                    "parent".to_owned(),
                    std::sync::Arc::new(tokio::sync::Mutex::new(turn)),
                ),
                distill_tools::implementations::distill::task::types::ActiveAgentMessageOperation::Queue,
                distill_tools::implementations::distill::task::types::ActiveAgentMessageOperation::Queue,
                None,
            ),
        },
    )
}

fn start_drain(
    capacity: usize,
    cancel_token: CancellationToken,
) -> (
    mpsc::Sender<PromptTurnReceipt>,
    PromptTurnReceiptDrain,
    mpsc::UnboundedReceiver<SessionCommand>,
) {
    let (handoff_tx, handoff_rx) = mpsc::channel(capacity);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    (
        handoff_tx,
        PromptTurnReceiptDrain::start(handoff_rx, cmd_tx, cancel_token),
        cmd_rx,
    )
}

#[tokio::test]
async fn receipts_settle_concurrently_while_preserving_fifo_authority() {
    let (handoff_tx, drain, _cmd_rx) = start_drain(1, CancellationToken::new());
    let (first_tx, first) = receipt_for_turn("parent-message-first", 3);
    let (second_tx, second) = receipt_for_turn("parent-message-second", 4);
    await_with_timeout(handoff_tx.send(first))
        .await
        .expect("first receipt handoff");
    await_with_timeout(handoff_tx.send(second))
        .await
        .expect("continuous drain replenishes handoff capacity");
    drop(handoff_tx);

    second_tx
        .send(crate::session::commands::ok_end_turn(2, None))
        .expect("second receipt settles");
    tokio::task::yield_now().await;
    first_tx
        .send(crate::session::commands::ok_end_turn(1, None))
        .expect("first receipt settles");
    let settled = await_with_timeout(drain.settle(AdmissionSettlement::Settled)).await;
    let Some(FinalPromptTurnReceipt {
        prompt_id,
        outcome: PromptTurnReceiptOutcome::Settled(receipt),
        telemetry,
    }) = settled.final_receipt
    else {
        panic!("expected successful final receipt");
    };
    let Ok(Ok(result)) = *receipt else {
        panic!("expected successful final receipt result");
    };
    assert_eq!(
        (prompt_id.as_str(), result.total_tokens, settled.disposition),
        (
            "parent-message-second",
            2,
            PromptTurnReceiptDisposition::Completed,
        ),
    );
    assert_eq!(*telemetry.parent_ctx.prompt_index.lock().await, 4);
}

#[tokio::test]
async fn active_drain_accepts_more_than_handoff_capacity() {
    let (handoff_tx, drain, _cmd_rx) =
        start_drain(ACTIVE_MESSAGE_RECEIPT_CAPACITY, CancellationToken::new());
    let lifetime_messages = ACTIVE_MESSAGE_RECEIPT_CAPACITY * 2 + 1;
    for index in 0..lifetime_messages {
        let prompt_id = format!("parent-message-{index}");
        let (result_tx, pending) = receipt(&prompt_id);
        await_with_timeout(handoff_tx.send(pending))
            .await
            .expect("continuous drain must prevent lifetime saturation");
        result_tx
            .send(crate::session::commands::ok_end_turn(index as u64, None))
            .expect("receipt settles");
    }
    drop(handoff_tx);
    let settled = await_with_timeout(drain.settle(AdmissionSettlement::Settled)).await;

    assert!(matches!(
        settled.final_receipt,
        Some(FinalPromptTurnReceipt {
            prompt_id,
            outcome: PromptTurnReceiptOutcome::Settled(receipt),
            ..
        }) if prompt_id == format!("parent-message-{}", lifetime_messages - 1)
            && matches!(receipt.as_ref(), Ok(Ok(_)))
    ));
    assert_eq!(settled.disposition, PromptTurnReceiptDisposition::Completed);
}

fn dropped_oneshot_error() -> oneshot::error::RecvError {
    let (tx, rx) = oneshot::channel::<crate::session::commands::PromptTurnResult>();
    drop(tx);
    futures::FutureExt::now_or_never(rx)
        .expect("closed oneshot is immediately ready")
        .expect_err("dropped sender yields RecvError")
}

#[test]
fn completed_settlement_preserves_successful_followup_for_parent_wake() {
    let cases = [
        (
            Some(Ok(crate::session::commands::ok_end_turn(1, None))),
            false,
            true,
            false,
            None,
            "follow-up result",
            false,
        ),
        (
            Some(Ok(crate::session::commands::ok_end_turn(1, None))),
            true,
            true,
            false,
            None,
            "follow-up result",
            false,
        ),
        (
            Some(Err(dropped_oneshot_error())),
            false,
            false,
            false,
            Some("Child session dropped unexpectedly"),
            "follow-up result",
            true,
        ),
        (
            Some(Err(dropped_oneshot_error())),
            true,
            false,
            true,
            Some("Subagent was cancelled"),
            "follow-up result",
            true,
        ),
        (None, false, false, true, None, "kept", false),
        (None, true, false, true, None, "kept", false),
    ];

    for (
        final_receipt,
        was_cancelled,
        success,
        cancelled,
        error,
        output,
        cancellation_may_hide_usage,
    ) in cases
    {
        let folded = reduce_prompt_turn_settlement(PromptTurnSettlementInput {
            result: SubagentResult {
                cancelled: true,
                output: std::sync::Arc::from("kept"),
                ..Default::default()
            },
            disposition: PromptTurnReceiptDisposition::Completed,
            final_receipt,
            final_text: "follow-up result".to_string(),
            was_cancelled,
        });

        assert_eq!(
            (
                folded.result.success,
                folded.result.cancelled,
                folded.result.error.as_deref(),
                folded.result.output.as_ref(),
                folded.cancellation_may_hide_usage,
            ),
            (
                success,
                cancelled,
                error,
                output,
                cancellation_may_hide_usage,
            ),
        );
    }
}

#[test]
fn unclean_settlement_dispositions_map_to_cancelled_results() {
    let cases = [
        (
            PromptTurnReceiptDisposition::Cancelled,
            "Subagent was cancelled",
        ),
        (
            PromptTurnReceiptDisposition::AdmissionUncertain,
            "Active-message admission could not be proven settled",
        ),
        (
            PromptTurnReceiptDisposition::TimedOut,
            "Subagent receipt settlement timed out",
        ),
    ];

    for (disposition, expected_error) in cases {
        let folded = reduce_prompt_turn_settlement(PromptTurnSettlementInput {
            result: SubagentResult {
                success: true,
                ..Default::default()
            },
            disposition,
            final_receipt: None,
            final_text: "partial".to_string(),
            was_cancelled: false,
        });

        assert_eq!(
            (
                folded.result.success,
                folded.result.cancelled,
                folded.result.error.as_deref(),
                folded.result.output.as_ref(),
                folded.result.output_usage_incomplete,
                folded.cancellation_may_hide_usage,
            ),
            (false, true, Some(expected_error), "partial", true, true),
        );
    }
}

#[tokio::test(start_paused = true)]
async fn clean_settlement_waits_past_legacy_timeout_without_shutdown() {
    let (handoff_tx, drain, mut cmd_rx) = start_drain(1, CancellationToken::new());
    let (receipt_tx, pending) = receipt("parent-message-long-running");
    await_with_timeout(handoff_tx.send(pending))
        .await
        .expect("receipt handoff");

    let settlement = tokio::spawn(drain.settle(AdmissionSettlement::Settled));
    await_with_timeout(handoff_tx.closed()).await;
    drop(handoff_tx);
    assert!(!settlement.is_finished());

    tokio::time::advance(std::time::Duration::from_secs(10 * 60 + 1)).await;
    tokio::task::yield_now().await;

    assert!(
        !settlement.is_finished(),
        "clean settlement must remain pending while the admitted turn runs"
    );
    assert!(cmd_rx.try_recv().is_err());

    receipt_tx
        .send(crate::session::commands::ok_end_turn(1, None))
        .expect("receipt settles");
    let settled = await_with_timeout(settlement)
        .await
        .expect("settlement task completes");

    assert_eq!(settled.disposition, PromptTurnReceiptDisposition::Completed);
    assert!(matches!(
        settled.final_receipt,
        Some(FinalPromptTurnReceipt {
            prompt_id,
            outcome: PromptTurnReceiptOutcome::Settled(receipt),
            ..
        }) if prompt_id == "parent-message-long-running"
            && matches!(receipt.as_ref(), Ok(Ok(_)))
    ));
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn cancellation_after_clean_settlement_arms_grace_deadline() {
    let cancel_token = CancellationToken::new();
    let (handoff_tx, drain, mut cmd_rx) = start_drain(1, cancel_token.clone());
    let (_receipt_tx, pending) = receipt("parent-message-cancelled-late");
    await_with_timeout(handoff_tx.send(pending))
        .await
        .expect("receipt handoff");

    let settlement = tokio::spawn(drain.settle(AdmissionSettlement::Settled));
    await_with_timeout(handoff_tx.closed()).await;
    drop(handoff_tx);
    assert!(!settlement.is_finished());

    tokio::time::advance(std::time::Duration::from_secs(10 * 60 + 1)).await;
    tokio::task::yield_now().await;
    assert!(cmd_rx.try_recv().is_err());

    let cancelled_at = tokio::time::Instant::now();
    cancel_token.cancel();
    assert!(matches!(
        await_with_timeout(cmd_rx.recv()).await,
        Some(SessionCommand::Cancel(_))
    ));
    assert!(cmd_rx.try_recv().is_err());

    tokio::time::advance(CANCELLED_RECEIPT_SETTLEMENT_GRACE - std::time::Duration::from_millis(1))
        .await;
    tokio::task::yield_now().await;
    assert!(cmd_rx.try_recv().is_err());

    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    assert!(matches!(
        await_with_timeout(cmd_rx.recv()).await,
        Some(SessionCommand::Shutdown(ShutdownKind::CancelRunningTurn))
    ));
    assert_eq!(
        tokio::time::Instant::now(),
        cancelled_at + CANCELLED_RECEIPT_SETTLEMENT_GRACE
    );

    let settled = await_with_timeout(settlement)
        .await
        .expect("settlement task completes");
    assert_eq!(settled.disposition, PromptTurnReceiptDisposition::TimedOut);
}

#[tokio::test]
async fn cancellation_drains_two_receipts_and_cancels_the_child_turn() {
    let cancel_token = CancellationToken::new();
    let (handoff_tx, drain, mut cmd_rx) = start_drain(2, cancel_token.clone());
    let (first_tx, first) = receipt("parent-message-first");
    let (second_tx, second) = receipt("parent-message-second");
    await_with_timeout(handoff_tx.send(first))
        .await
        .expect("first receipt handoff");
    await_with_timeout(handoff_tx.send(second))
        .await
        .expect("second receipt handoff");
    drop(handoff_tx);
    cancel_token.cancel();

    let mut settlement = Box::pin(drain.settle(AdmissionSettlement::Settled));
    assert!(
        tokio::time::timeout(std::time::Duration::ZERO, &mut settlement)
            .await
            .is_err(),
        "cancellation starts teardown but preserves receipt draining"
    );
    assert!(matches!(
        await_with_timeout(cmd_rx.recv()).await,
        Some(SessionCommand::Cancel(_))
    ));
    first_tx
        .send(crate::session::commands::ok_end_turn(1, None))
        .expect("first receipt settles");
    second_tx
        .send(crate::session::commands::ok_end_turn(2, None))
        .expect("second receipt settles");

    let settled = await_with_timeout(settlement).await;
    assert_eq!(settled.disposition, PromptTurnReceiptDisposition::Cancelled);
    assert!(matches!(
        settled.final_receipt,
        Some(FinalPromptTurnReceipt {
            prompt_id,
            outcome: PromptTurnReceiptOutcome::Settled(receipt),
            ..
        }) if prompt_id == "parent-message-second"
            && matches!(receipt.as_ref(), Ok(Ok(_)))
    ));
}

#[tokio::test(start_paused = true)]
async fn uncertain_settlement_deadline_forces_turn_shutdown() {
    let (handoff_tx, drain, mut cmd_rx) = start_drain(1, CancellationToken::new());
    let (_receipt_tx, pending) = receipt("parent-message-uncertain");
    await_with_timeout(handoff_tx.send(pending))
        .await
        .expect("receipt handoff");
    drop(handoff_tx);

    let settlement = tokio::spawn(drain.settle(AdmissionSettlement::Uncertain));
    assert!(matches!(
        await_with_timeout(cmd_rx.recv()).await,
        Some(SessionCommand::Cancel(_))
    ));

    tokio::time::advance(CANCELLED_RECEIPT_SETTLEMENT_GRACE - std::time::Duration::from_millis(1))
        .await;
    tokio::task::yield_now().await;
    assert!(!settlement.is_finished());
    assert!(cmd_rx.try_recv().is_err());

    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    assert!(matches!(
        await_with_timeout(cmd_rx.recv()).await,
        Some(SessionCommand::Shutdown(ShutdownKind::CancelRunningTurn))
    ));
    let settled = await_with_timeout(settlement)
        .await
        .expect("settlement task completes");
    assert_eq!(settled.disposition, PromptTurnReceiptDisposition::TimedOut);
    assert!(matches!(
        settled.final_receipt,
        Some(FinalPromptTurnReceipt {
            prompt_id,
            outcome: PromptTurnReceiptOutcome::TimedOut,
            ..
        }) if prompt_id == "parent-message-uncertain"
    ));
}

#[tokio::test(start_paused = true)]
async fn cancellation_deadline_forces_turn_shutdown() {
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();
    let (handoff_tx, drain, mut cmd_rx) = start_drain(1, cancel_token);
    let (_receipt_tx, pending) = receipt("parent-message-pending");
    await_with_timeout(handoff_tx.send(pending))
        .await
        .expect("receipt handoff");
    drop(handoff_tx);

    let settled = await_with_timeout(drain.settle(AdmissionSettlement::Settled)).await;

    assert_eq!(settled.disposition, PromptTurnReceiptDisposition::TimedOut);
    assert!(matches!(
        settled.final_receipt,
        Some(FinalPromptTurnReceipt {
            prompt_id,
            outcome: PromptTurnReceiptOutcome::TimedOut,
            ..
        }) if prompt_id == "parent-message-pending"
    ));
    assert!(matches!(
        await_with_timeout(cmd_rx.recv()).await,
        Some(SessionCommand::Cancel(_))
    ));
    assert!(matches!(
        await_with_timeout(cmd_rx.recv()).await,
        Some(SessionCommand::Shutdown(ShutdownKind::CancelRunningTurn))
    ));
}

#[tokio::test]
async fn actor_drop_settles_as_a_closed_receipt() {
    let (handoff_tx, drain, _cmd_rx) = start_drain(1, CancellationToken::new());
    let (receipt_tx, dropped) = receipt("parent-message-dropped");
    await_with_timeout(handoff_tx.send(dropped))
        .await
        .expect("receipt handoff");
    drop(handoff_tx);
    drop(receipt_tx);

    let settled = await_with_timeout(drain.settle(AdmissionSettlement::Settled)).await;

    assert!(matches!(
        settled.final_receipt,
        Some(FinalPromptTurnReceipt {
            prompt_id,
            outcome: PromptTurnReceiptOutcome::Settled(receipt),
            ..
        }) if prompt_id == "parent-message-dropped" && receipt.is_err()
    ));
    assert_eq!(settled.disposition, PromptTurnReceiptDisposition::Completed);
}
