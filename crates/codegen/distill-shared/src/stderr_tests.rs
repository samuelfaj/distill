use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{stderr_lock, try_with_locked_stderr_for};

/// The holder is released before any assertion so a failure cannot leave the process-global lock held for the rest of
/// the test binary.
#[test]
fn gives_up_while_the_lock_is_held() {
    let (held_tx, held_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = stderr_lock();
        held_tx.send(()).expect("signal held");
        let _ = release_rx.recv();
    });
    held_rx.recv().expect("holder took the lock");

    let mut ran = false;
    let started = Instant::now();
    let while_held = try_with_locked_stderr_for(Duration::from_millis(50), |_| ran = true);
    let elapsed = started.elapsed();
    let _ = release_tx.send(());
    holder.join().expect("holder thread");

    assert_eq!(None, while_held);
    assert!(!ran, "callback ran without the lock");
    assert!(
        elapsed < Duration::from_secs(2),
        "gave up late: {elapsed:?}"
    );

    let after_release = try_with_locked_stderr_for(Duration::from_secs(5), |_| ran = true);
    assert_eq!(Some(()), after_release);
    assert!(ran, "callback did not run once the lock was free");
}
