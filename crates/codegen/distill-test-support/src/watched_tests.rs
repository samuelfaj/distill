use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::*;

#[tokio::test(start_paused = true)]
async fn an_update_after_the_wait_started_wakes_it_before_the_deadline() {
    let watched = Arc::new(Watched::new(0));
    let start = Instant::now();
    let deadline = start + Duration::from_secs(60);
    let writer = Arc::clone(&watched);

    let (outcome, ()) = tokio::join!(
        watched.wait_until(
            deadline,
            |count| *count,
            |count| (*count == 1).then_some(*count)
        ),
        async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            writer.update(|count| *count += 1);
        },
    );

    assert_eq!(
        (WaitOutcome::Accepted(1), Duration::from_secs(1)),
        (outcome, start.elapsed())
    );
}

#[tokio::test(start_paused = true)]
async fn without_an_update_the_deadline_passes_with_the_snapshot_last_rejected() {
    let watched = Watched::new(7);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(60);

    let outcome = watched
        .wait_until(
            deadline,
            |count| *count,
            |count| (*count == 8).then_some(*count),
        )
        .await;

    assert_eq!(
        (WaitOutcome::DeadlinePassed(7), Duration::from_secs(60)),
        (outcome, start.elapsed())
    );
}
