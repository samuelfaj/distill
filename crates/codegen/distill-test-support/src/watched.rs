//! A double's state behind one lock whose every write wakes the tests waiting on it.

use std::sync::Mutex;

use tokio::sync::watch;
use tokio::time::Instant;

pub(crate) struct Watched<T> {
    state: Mutex<T>,
    changed: watch::Sender<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome<R, S> {
    Accepted(R),
    /// Holds the snapshot `accept` rejected last, so a caller reports the state it did not accept.
    DeadlinePassed(S),
}

impl<T> Watched<T> {
    pub(crate) fn new(state: T) -> Watched<T> {
        let (changed, _) = watch::channel(0);
        Watched {
            state: Mutex::new(state),
            changed,
        }
    }

    pub(crate) fn update<R>(&self, write: impl FnOnce(&mut T) -> R) -> R {
        let result = write(&mut self.state.lock().unwrap());
        self.changed.send_modify(|revision| *revision += 1);
        result
    }

    pub(crate) fn read<R>(&self, view: impl FnOnce(&T) -> R) -> R {
        view(&self.state.lock().unwrap())
    }

    /// Probes a snapshot now, after every update, and once more when `deadline` passes. `view`
    /// runs under the lock, so it must not call back into the double; `accept` runs with the lock
    /// released, so it may.
    pub(crate) async fn wait_until<S, R>(
        &self,
        deadline: Instant,
        view: impl Fn(&T) -> S,
        mut accept: impl FnMut(&S) -> Option<R>,
    ) -> WaitOutcome<R, S> {
        let mut changes = self.changed.subscribe();
        let mut deadline_passed = false;
        loop {
            let snapshot = self.read(&view);
            if let Some(found) = accept(&snapshot) {
                return WaitOutcome::Accepted(found);
            }
            if deadline_passed {
                return WaitOutcome::DeadlinePassed(snapshot);
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => unreachable!("the sender lives in the state this wait borrows"),
                Err(_) => deadline_passed = true,
            }
        }
    }
}

#[cfg(test)]
#[path = "watched_tests.rs"]
mod tests;
