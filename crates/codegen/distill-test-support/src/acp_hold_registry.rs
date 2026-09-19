//! The requests from the agent that a `HoldUntilCancel` decision keeps open, keyed by session. Each hold is
//! registered by a guard, so a handler dropped mid hold leaves no entry.

use std::sync::atomic::{AtomicUsize, Ordering};

use agent_client_protocol as acp;
use tokio::sync::watch;

#[derive(Clone, Copy, PartialEq, Eq)]
enum HoldState {
    Waiting,
    Released,
}

struct Hold {
    id: usize,
    session_id: acp::SessionId,
    state: HoldState,
}

#[derive(Default)]
pub(crate) struct HoldRegistry {
    next_id: AtomicUsize,
    /// The watch is both the lock over the holds and what wakes a waiter; each wait subscribes its own receiver.
    holds: watch::Sender<Vec<Hold>>,
}

/// Keeps its hold registered until dropped.
#[must_use]
pub(crate) struct RegisteredHold<'a> {
    registry: &'a HoldRegistry,
    id: usize,
}

impl Drop for RegisteredHold<'_> {
    fn drop(&mut self) {
        self.registry
            .holds
            .send_modify(|holds| holds.retain(|hold| hold.id != self.id));
    }
}

impl HoldRegistry {
    /// Holds a request for `session_id` until the session is next released, then returns the guard that keeps
    /// the hold registered.
    pub(crate) async fn hold_until_released(
        &self,
        session_id: &acp::SessionId,
    ) -> RegisteredHold<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.holds.send_modify(|holds| {
            holds.push(Hold {
                id,
                session_id: session_id.clone(),
                state: HoldState::Waiting,
            });
        });
        let registered = RegisteredHold { registry: self, id };
        self.wait_until(|holds| {
            holds
                .iter()
                .any(|hold| hold.id == id && hold.state == HoldState::Released)
        })
        .await;
        registered
    }

    /// Releases every request held for `session_id` at this moment and resolves once each released guard is
    /// dropped, which a handler does only after recording its reply. A request held later waits for the next
    /// release.
    pub(crate) async fn release_held_requests(&self, session_id: &acp::SessionId) {
        self.holds.send_modify(|holds| {
            for hold in holds
                .iter_mut()
                .filter(|hold| hold.session_id == *session_id)
            {
                hold.state = HoldState::Released;
            }
        });
        self.wait_until(|holds| {
            !holds
                .iter()
                .any(|hold| hold.session_id == *session_id && hold.state == HoldState::Released)
        })
        .await;
    }

    pub(crate) async fn wait_for_held_request(&self, session_id: &acp::SessionId) {
        self.wait_until(|holds| holds.iter().any(|hold| hold.session_id == *session_id))
            .await;
    }

    async fn wait_until(&self, is_done: impl Fn(&[Hold]) -> bool) {
        self.holds
            .subscribe()
            .wait_for(|holds| is_done(holds))
            .await
            .expect("the registry owns the sender for the whole wait");
    }
}

#[cfg(test)]
#[path = "acp_hold_registry_tests.rs"]
mod tests;
