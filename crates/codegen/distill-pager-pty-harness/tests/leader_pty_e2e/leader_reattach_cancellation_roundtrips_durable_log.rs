// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `leader_pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// The leader must persist a `turn_completed` terminal with `stop_reason == cancelled`. A fresh client must replay
/// the cancelled transcript through the same leader and land clean. It must still hold that transcript after A, the
/// original driver, exits. C attaches *before* A is dropped, because `PtyHarness` Drop SIGKILLs the child.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "PTY e2e; run with cargo test -p distill-pager-pty-harness --test leader_pty_e2e -- --ignored --test-threads=1"]
async fn leader_reattach_cancellation_roundtrips_durable_log() {
    let cluster = LeaderCluster::start(DEFAULT_ROWS, DEFAULT_COLS)
        .await
        .expect("start leader cluster");
    // The response is paced enough to cancel mid-stream and short enough that the heavy multi-client cancel drain does not dominate the suite
    let long_response = format!(
        "{} {}",
        turn_sentinel(1),
        "more streamed filler to hold the turn open. ".repeat(40)
    );
    cluster.content().set_response(long_response);
    cluster
        .content()
        .set_chunk_delay(Some(Duration::from_millis(40)));

    let mut a = cluster.spawn_leader(&[]).expect("spawn leader client A");
    a.wait_for_text(WELCOME_SCREEN_SENTINEL, LEADER_TIMEOUT)
        .expect("A welcome");
    a.inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("A submit turn");
    // Wait until the turn is clearly streaming (sentinel visible); this also closes the leader's rewind window so cancel is not confused with rewind
    a.wait_for_text(&turn_sentinel(1), STREAM_TIMEOUT)
        .expect("A turn streaming");

    // Ctrl+C on an empty prompt cancels while streaming.
    a.inject_keys(keys::CTRL_C).expect("A press ctrl+c");
    a.update(Duration::from_millis(200));
    // Generous budget: the heavy multi-client leader cluster drains the paced cancel slower than the single-client path
    // Match the test's other waits (LEADER/STREAM_TIMEOUT) rather than the single-client 15s
    a.wait_for_text("Turn cancelled by user", STREAM_TIMEOUT)
        .expect("A turn cancelled marker");

    // The cancel must have persisted a durable terminal carrying the cancelled stop reason
    let rec = cluster
        .wait_for_turn_completed(STREAM_TIMEOUT)
        .expect("turn_completed persisted to updates.jsonl");
    assert_eq!(
        rec["stop_reason"], "cancelled",
        "cancelled turn must record stop_reason=cancelled, got {rec}"
    );

    // The keep-alive viewer attaches AFTER the cancel so the leader survives A's exit
    // Waiting for it to replay proves it is attached to the same session
    let mut keep = cluster.attach(&[]).expect("spawn keep-alive viewer");
    keep.wait_for_text(&turn_sentinel(1), LEADER_TIMEOUT)
        .expect("keep-alive replayed the cancelled transcript");

    // Reattach must replay from the durable log, not re-drive a turn: the mock must see no new inference request while C catches up
    let inference_before_reattach = inference_request_count(cluster.content());

    // Fresh reattach while A is still up (see module comment), then prove the original driver's exit does not take C (or the transcript) down
    let mut c = cluster.attach(&[]).expect("spawn fresh reattach client C");
    c.wait_for_text(&turn_sentinel(1), LEADER_TIMEOUT)
        .expect("C replayed the cancelled transcript");

    drop(a);
    // The leader processes A's disconnect; settle briefly, then re-check C still holds the durable replay (a fixed long sleep would mask a hang)
    c.update(Duration::from_millis(500));
    assert!(
        c.is_running().expect("poll pager liveness"),
        "C exited after A quit\nscreen:\n{}",
        c.screen_contents()
    );
    assert!(
        c.contains_text(&turn_sentinel(1)),
        "C lost the cancelled transcript after A quit\nscreen:\n{}",
        c.screen_contents()
    );

    // As in the completion test, a fresh reattach lands Idle either way, so the spinner checks guard regressions rather than prove the replay
    // Bare substrings (not the full `…`-suffixed labels) are simple stable matches for the spinner labels
    // The wait for the labels to disappear is bounded rather than a fixed settle
    wait_for_labels_absent(&mut c, &["Waiting", "Cancelling"], Duration::from_secs(5));

    assert!(
        !c.contains_text("panicked"),
        "C rendered a panic\nscreen:\n{}",
        c.screen_contents()
    );
    assert!(
        !c.contains_text("Waiting"),
        "C is stranded on the active-turn spinner\nscreen:\n{}",
        c.screen_contents()
    );
    assert!(
        !c.contains_text("Cancelling"),
        "C is stranded on the cancelling spinner\nscreen:\n{}",
        c.screen_contents()
    );
    assert_eq!(
        inference_request_count(cluster.content()),
        inference_before_reattach,
        "reattach must replay from the durable log, not re-drive a turn (no new inference request)"
    );

    c.quit().expect("quit C");
    keep.quit().expect("quit keep-alive viewer");
}
