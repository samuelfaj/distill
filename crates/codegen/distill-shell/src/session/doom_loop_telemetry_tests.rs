// Modified for Distill by Samuel Fajreldines, 2026.
use super::{
    DoomLoopDetectionSummary, DoomLoopTurnTally, MAX_TRIGGER_LABEL_BYTES, MAX_TRIGGER_LABELS,
    reconcile_request_metadata,
};

#[test]
fn tally_dedupes_and_summarizes_detector_kinds() {
    let mut tally = DoomLoopTurnTally::default();
    tally.merge_all_triggers(&[
        "tail_repetition:64@thinking".to_owned(),
        "exact_repetition:42x3@response".to_owned(),
    ]);
    tally.merge_all_triggers(&[
        "tail_repetition:64@thinking".to_owned(),
        "tail_repetition:32@thinking".to_owned(),
        "exact_repetition:24x7@response".to_owned(),
    ]);

    assert_eq!(4, tally.triggers.len());
    assert!(tally.detected());
    assert!(!tally.fired());
    assert_eq!(
        DoomLoopDetectionSummary {
            detector_kinds: vec!["tail_repetition".to_owned(), "exact_repetition".to_owned()],
            channels: vec!["thinking".to_owned(), "response".to_owned()],
            tightest_tail_threshold: Some(32),
            max_exact_sequence_tokens: Some(42),
            max_exact_repeat_count: Some(7),
        },
        tally.detection_summary(),
    );
}

#[test]
fn summary_collapses_untrusted_channels_to_other() {
    let mut tally = DoomLoopTurnTally::default();
    tally.merge_all_triggers(&[
        "tail_repetition:8@thinking".to_owned(),
        "exact_repetition:42x3@model-generated-private-text".to_owned(),
        "low_logprob@".to_owned(),
    ]);

    assert_eq!(
        vec!["thinking".to_owned(), "other".to_owned()],
        tally.detection_summary().channels,
    );
}

#[test]
fn trigger_retention_is_bounded_by_count_and_bytes() {
    let mut tally = DoomLoopTurnTally::default();
    let labels: Vec<String> = (0..MAX_TRIGGER_LABELS + 20)
        .map(|index| format!("unknown_{index}@thinking"))
        .collect();
    tally.merge_all_triggers(&labels);
    assert_eq!(MAX_TRIGGER_LABELS, tally.triggers.len());

    let overlong = "x".repeat(MAX_TRIGGER_LABEL_BYTES + 1);
    tally.merge_all_triggers(&[overlong]);
    assert_eq!(MAX_TRIGGER_LABELS, tally.triggers.len());
    assert!(
        tally
            .triggers
            .iter()
            .all(|trigger| trigger.len() <= MAX_TRIGGER_LABEL_BYTES)
    );
}

#[test]
fn unowned_awaited_metadata_cannot_rebook_after_cancel_reset() {
    let mut tally = DoomLoopTurnTally::default();
    let triggers = vec!["tail_repetition:8@thinking".to_owned()];
    let attempts = vec![distill_sampler::DoomLoopRecoveryAttempt {
        triggers: triggers.clone(),
        aborted_at_chunk: Some(42),
    }];

    assert!(tally.record_recovery_attempt("request-a", 1, &triggers));
    tally = DoomLoopTurnTally::default();
    let counted = reconcile_request_metadata(&mut tally, false, "request-a", &triggers, &attempts);

    assert!(counted.is_empty());
    assert_eq!(0, tally.attempts);
    assert!(tally.triggers.is_empty());
}

#[test]
fn request_scoped_idempotence_counts_each_action_once() {
    let mut tally = DoomLoopTurnTally::default();
    let triggers = vec!["tail_repetition:8@thinking".to_owned()];
    assert!(tally.record_recovery_attempt("request-a", 1, &triggers));
    assert!(!tally.record_recovery_attempt("request-a", 1, &triggers));
    assert!(tally.record_recovery_attempt("request-b", 1, &triggers));
    assert_eq!(2, tally.attempts);

    assert!(tally.mark_recovery_attempt_stamped("request-a", 1));
    assert!(!tally.mark_recovery_attempt_stamped("request-a", 1));
    assert!(tally.mark_accepted_request("request-a"));
    assert!(!tally.mark_accepted_request("request-a"));
    assert!(tally.mark_accepted_request("request-b"));
    assert!(tally.mark_accepted_request_stamped("request-a"));
    assert!(!tally.mark_accepted_request_stamped("request-a"));
}
