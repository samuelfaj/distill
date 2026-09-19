use super::*;

#[test]
fn bounded_log_keeps_the_newest_entries_past_its_cap() {
    let mut pushed = BoundedLog::new(3);
    for entry in 1..=5 {
        pushed.push(entry);
    }
    let mut extended = BoundedLog::new(3);
    extended.extend(1..=4);

    assert_eq!(
        ((vec![3, 4, 5], 2), (vec![2, 3, 4], 1)),
        (
            (pushed.to_vec(), pushed.evicted()),
            (extended.to_vec(), extended.evicted())
        )
    );
}

#[test]
fn arrived_since_counts_evicted_arrivals_and_returns_the_retained_ones() {
    let mut log = BoundedLog::new(3);
    log.extend(1..=2);
    let baseline = log.total();
    log.extend(3..=6);

    assert_eq!(
        (6, vec![4, 5, 6]),
        (log.total(), log.arrived_since(baseline))
    );
}
