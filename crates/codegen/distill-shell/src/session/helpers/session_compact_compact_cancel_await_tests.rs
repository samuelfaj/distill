use super::*;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn pre_cancelled_token_skips_fut() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = await_unless_cancelled(&cancel, async {
        panic!("fut must not run when already cancelled");
    })
    .await
    .unwrap_err();
    assert!(matches!(err, CompactFailure::Cancelled));
}

#[tokio::test]
async fn cancel_aborts_pending_open() {
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel2.cancel();
    });
    let started = std::time::Instant::now();
    let err = await_unless_cancelled(&cancel, async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        0u8
    })
    .await
    .unwrap_err();
    assert!(matches!(err, CompactFailure::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stop must abort stream-open wait, elapsed {:?}",
        started.elapsed()
    );
}

#[test]
fn chat_compaction_keeps_normalized_free_cost_and_known_cost_on_legacy_empty_chunks() {
    let mut response_id = Some("known-response".to_owned());
    let mut response_model = Some("known-model".to_owned());
    retain_nonempty_identity(&mut response_id, "");
    retain_nonempty_identity(&mut response_model, "");
    assert_eq!(response_id.as_deref(), Some("known-response"));
    assert_eq!(response_model.as_deref(), Some("known-model"));
    retain_nonempty_identity(&mut response_id, "later-response");
    retain_nonempty_identity(&mut response_model, "later-model");
    assert_eq!(response_id.as_deref(), Some("later-response"));
    assert_eq!(response_model.as_deref(), Some("later-model"));

    let authoritative_free = distill_sampling_types::Usage {
        prompt_tokens: 1,
        completion_tokens: 1,
        total_tokens: 2,
        prompt_tokens_details: None,
        completion_tokens_details: None,
        cost_in_usd_ticks: None,
        cost: Some(0.0),
    };
    assert_eq!(
        merge_normalized_cost(None, &authoritative_free),
        Some(0),
        "an authoritative USD zero is a reported free call"
    );
    assert_eq!(
        jev_billing_from_tokens(&distill_sampling_types::TokenUsage::default(), Some(0))
            .cost_usd_ticks,
        Some(0),
        "billing must not turn normalized free cost into unknown"
    );

    let legacy_empty = distill_sampling_types::Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        prompt_tokens_details: None,
        completion_tokens_details: None,
        cost_in_usd_ticks: Some(0),
        cost: None,
    };
    assert_eq!(
        merge_normalized_cost(Some(99), &legacy_empty),
        Some(99),
        "legacy empty usage must not clobber a known earlier cost"
    );
}
