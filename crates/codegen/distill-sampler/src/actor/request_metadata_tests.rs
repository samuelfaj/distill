use super::{CompletionState, merge_signal_labels};
use crate::doom_loop::{MAX_COLLECTED_DOOM_LOOP_SIGNALS, MAX_DOOM_LOOP_SIGNAL_BYTES};

#[test]
fn signal_labels_are_bounded_by_count_and_bytes() {
    let mut labels = Vec::new();
    merge_signal_labels(
        &mut labels,
        (0..MAX_COLLECTED_DOOM_LOOP_SIGNALS + 20).map(|index| format!("unknown_{index}@thinking")),
    );
    assert_eq!(MAX_COLLECTED_DOOM_LOOP_SIGNALS, labels.len());

    labels.clear();
    merge_signal_labels(&mut labels, ["x".repeat(MAX_DOOM_LOOP_SIGNAL_BYTES + 1)]);
    assert!(labels.is_empty());
}

#[test]
fn recovery_attempts_are_bounded() {
    let mut completion = CompletionState::new(None);
    for _ in 0..100 {
        completion.record_recovery_attempt(["tail_repetition:8@thinking".to_owned()], None);
    }
    assert_eq!(16, completion.doom_loop_recovery_attempts.len());
}
