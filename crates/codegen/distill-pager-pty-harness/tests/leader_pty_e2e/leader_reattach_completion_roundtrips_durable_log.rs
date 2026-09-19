// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `leader_pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// the leader must persist a replayable `turn_completed` record. If the leader never writes that record, the test
/// fails right there, before any replay. It must land clean: running, no panic, and not stranded on the active-turn
/// "Waiting" spinner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "PTY e2e; run with cargo test -p distill-pager-pty-harness --test leader_pty_e2e -- --ignored --test-threads=1"]
async fn leader_reattach_completion_roundtrips_durable_log() {
    let cluster = LeaderCluster::start(DEFAULT_ROWS, DEFAULT_COLS)
        .await
        .expect("start leader cluster");
    cluster
        .content()
        .set_response(format!("{} completed turn payload.", turn_sentinel(1)));

    // A elects the leader and drives one turn to completion.
    let mut a = cluster.spawn_leader(&[]).expect("spawn leader client A");
    a.wait_for_text(WELCOME_SCREEN_SENTINEL, LEADER_TIMEOUT)
        .expect("A welcome");
    a.inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("A submit turn");
    a.wait_for_text(&turn_sentinel(1), STREAM_TIMEOUT)
        .expect("A turn rendered");

    // Attach the keep-alive viewer AFTER the turn so the leader survives A's exit
    // Waiting for it to replay the transcript proves it is attached
    let mut keep = cluster.attach(&[]).expect("spawn keep-alive viewer");
    keep.wait_for_text(&turn_sentinel(1), LEADER_TIMEOUT)
        .expect("keep-alive replayed A's transcript");

    // The completed turn must have persisted a durable, replayable `turn_completed` record carrying the real stop reason
    let rec = cluster
        .wait_for_turn_completed(STREAM_TIMEOUT)
        .expect("turn_completed persisted to updates.jsonl");
    assert_eq!(
        rec["stop_reason"], "end_turn",
        "completed turn must record stop_reason=end_turn, got {rec}"
    );
    assert!(
        rec["prompt_id"].as_str().is_some_and(|s| !s.is_empty()),
        "turn_completed must carry a non-empty prompt_id, got {rec}"
    );

    // Reattach must replay from the durable log, not re-drive a turn: the mock must see no new inference request while C catches up
    let inference_before_reattach = inference_request_count(cluster.content());

    // Fresh reattach AFTER A exits: C replays the completed transcript from the durable log through the surviving leader
    drop(a);
    let mut c = cluster.attach(&[]).expect("spawn fresh reattach client C");
    c.wait_for_text(&turn_sentinel(1), LEADER_TIMEOUT)
        .expect("C replayed the completed transcript");

    // A finished turn clears the leader's prompt slot, so a fresh reattach lands Idle even without the guard in the
    // replaying client. The "Waiting" check therefore guards against a regression rather than proving the feature. The
    // wait is bounded rather than a fixed sleep, which could flake under load.
    wait_for_labels_absent(&mut c, &["Waiting"], Duration::from_secs(5));

    assert!(
        c.is_running().expect("poll pager liveness"),
        "C exited unexpectedly\nscreen:\n{}",
        c.screen_contents()
    );
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
    assert_eq!(
        inference_request_count(cluster.content()),
        inference_before_reattach,
        "reattach must replay from the durable log, not re-drive a turn (no new inference request)"
    );
    // Pump once more so a late or duplicate replay batch would be painted before the exactly-once count below, the check this test exists for
    c.update(Duration::from_millis(500));
    let screen = c.screen_contents();
    assert_eq!(
        screen.matches(&turn_sentinel(1)).count(),
        1,
        "C must replay the turn exactly once (duplicated replay?)\nscreen:\n{screen}"
    );

    c.quit().expect("quit C");
    keep.quit().expect("quit keep-alive viewer");
}
