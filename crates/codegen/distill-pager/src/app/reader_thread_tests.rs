use std::time::{Duration, Instant};

use super::{ReaderJoin, ReaderThread};

#[test]
fn joins_a_finished_thread() {
    let reader = ReaderThread {
        handle: Some(std::thread::spawn(|| {})),
    };

    assert_eq!(
        ReaderJoin::Joined,
        reader.join_within(Duration::from_secs(2))
    );
}

/// The fixture blocks on a channel until the assertions are done, so it cannot finish early and turn `TimedOut` into `Joined`.
#[test]
fn detaches_a_running_thread_at_the_deadline() {
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let reader = ReaderThread {
        handle: Some(std::thread::spawn(move || {
            let _ = release_rx.recv();
        })),
    };
    let started = Instant::now();

    let join = reader.join_within(Duration::from_millis(50));

    assert_eq!(ReaderJoin::TimedOut, join);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "detach took {:?}",
        started.elapsed()
    );
    let _ = release_tx.send(());
}

#[test]
fn detached_is_absent() {
    assert_eq!(
        ReaderJoin::Absent,
        ReaderThread::detached().join_within(Duration::from_millis(50))
    );
}
