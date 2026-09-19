// Modified for Distill by Samuel Fajreldines, 2026.
//! Mid-session settings refresh coalescer.
//!
//! Concurrent live refreshes for one credential share a single in-flight fetch;
//! nothing is cached here (cross-launch reuse is the disk cache's job, see
//! `settings_cache`).

use distill_login::GrokAuth;
use tokio::sync::watch;

/// Coalesces concurrent mid-session settings refreshes onto one live fetch.
#[derive(Default)]
pub(in crate::agent) struct SettingsRefresh {
    state: std::cell::RefCell<RefreshState>,
}

#[derive(Clone, PartialEq, Eq)]
struct CredentialIdentity {
    user_id: String,
    key: String,
}

impl From<&GrokAuth> for CredentialIdentity {
    fn from(auth: &GrokAuth) -> Self {
        Self {
            user_id: auth.user_id.clone(),
            key: auth.key.clone(),
        }
    }
}

#[derive(Default)]
struct RefreshState {
    /// Bumped on every credential switch and never reset, so an A->B->A churn
    /// gives a new epoch and a stale leader cannot be taken for the live flight.
    epoch: u64,
    identity: Option<CredentialIdentity>,
    in_flight: Option<watch::Receiver<Option<crate::remote::SettingsFetch>>>,
}

impl RefreshState {
    fn reset_to(&mut self, identity: CredentialIdentity) {
        self.epoch += 1;
        self.identity = Some(identity);
        self.in_flight = None;
    }
}

enum RefreshPlan {
    Join(watch::Receiver<Option<crate::remote::SettingsFetch>>),
    Lead(watch::Sender<Option<crate::remote::SettingsFetch>>),
}

impl SettingsRefresh {
    const MAX_DROPPED_LEADER_REATTEMPTS: u32 = 3;

    /// Coalesce concurrent live refreshes for `auth` onto one `leader` fetch:
    /// the first caller leads and runs the fetch inline (it may borrow the
    /// caller and be `!Send`); the rest join its published outcome. `None` means
    /// the leader kept dropping before it published, so the caller must not
    /// resolve the fail-closed OTEL gate on cancellation churn.
    pub(in crate::agent) async fn refresh<F, Fut>(
        &self,
        auth: &GrokAuth,
        leader: F,
    ) -> Option<crate::remote::SettingsFetch>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = crate::remote::SettingsFetch>,
    {
        let identity = CredentialIdentity::from(auth);
        let mut leader = Some(leader);
        let mut dropped_leader_reattempts = 0u32;
        loop {
            let (plan, my_epoch) = {
                let mut st = self.state.borrow_mut();
                if st.identity.as_ref() != Some(&identity) {
                    st.reset_to(identity.clone());
                }
                let my_epoch = st.epoch;
                let plan = if let Some(rx) = st.in_flight.as_ref() {
                    RefreshPlan::Join(rx.clone())
                } else {
                    let (tx, rx) = watch::channel(None);
                    st.in_flight = Some(rx);
                    RefreshPlan::Lead(tx)
                };
                (plan, my_epoch)
            };

            match plan {
                RefreshPlan::Join(mut rx) => {
                    let published = loop {
                        if let Some(outcome) = rx.borrow_and_update().clone() {
                            break Some(outcome);
                        }
                        if rx.changed().await.is_err() {
                            break None;
                        }
                    };
                    match published {
                        Some(outcome) => return Some(outcome),
                        None => {
                            dropped_leader_reattempts += 1;
                            if dropped_leader_reattempts > Self::MAX_DROPPED_LEADER_REATTEMPTS {
                                return None;
                            }
                        }
                    }
                }
                RefreshPlan::Lead(tx) => {
                    let mut guard = RefreshLeaderGuard {
                        refresh: self,
                        epoch: my_epoch,
                        published: false,
                    };
                    let run = leader.take().expect("leader closure consumed at most once");
                    let outcome = run().await;
                    {
                        let mut st = self.state.borrow_mut();
                        if st.epoch == my_epoch {
                            st.in_flight = None;
                        }
                    }
                    guard.published = true;
                    let _ = tx.send(Some(outcome.clone()));
                    return Some(outcome);
                }
            }
        }
    }
}

struct RefreshLeaderGuard<'a> {
    refresh: &'a SettingsRefresh,
    epoch: u64,
    published: bool,
}

impl Drop for RefreshLeaderGuard<'_> {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let mut st = self.refresh.state.borrow_mut();
        if st.epoch == self.epoch {
            st.in_flight = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SettingsRefresh;
    use crate::remote::SettingsFetch;

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_coalesces_concurrent_callers() {
        let refresh = SettingsRefresh::default();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let leader = || {
            let calls = calls.clone();
            move || async move {
                calls.set(calls.get() + 1);
                tokio::task::yield_now().await;
                SettingsFetch::Fetched(Box::default())
            }
        };
        let auth = distill_login::GrokAuth::test_default();

        let cluster = (0..5).map(|_| refresh.refresh(&auth, leader()));
        let outcomes = futures::future::join_all(cluster).await;
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, Some(SettingsFetch::Fetched(_)))),
            "every coalesced caller receives the shared success"
        );
        assert_eq!(calls.get(), 1, "concurrent callers share one leader fetch");

        let _ = refresh.refresh(&auth, leader()).await;
        assert_eq!(
            calls.get(),
            2,
            "a sequential trigger re-fetches; no stale replay"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_leader_refetches_instead_of_gate_opening_retry() {
        let refresh = SettingsRefresh::default();
        let auth = distill_login::GrokAuth::test_default();

        let mut leader = Box::pin(refresh.refresh(&auth, std::future::pending::<SettingsFetch>));
        tokio::select! {
            biased;
            _ = &mut leader => unreachable!("a pending leader cannot complete"),
            _ = tokio::task::yield_now() => {}
        }

        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let calls_follower = calls.clone();
        let mut follower = Box::pin(refresh.refresh(&auth, move || async move {
            calls_follower.set(calls_follower.get() + 1);
            SettingsFetch::Fetched(Box::default())
        }));
        tokio::select! {
            biased;
            _ = &mut follower => unreachable!("the follower must park on the in-flight leader"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(calls.get(), 0, "a joining follower must not fetch");

        drop(leader);

        assert!(
            matches!(follower.await, Some(SettingsFetch::Fetched(_))),
            "a cancelled leader drives a real re-fetch, not a gate-opening Retry"
        );
        assert_eq!(
            calls.get(),
            1,
            "the follower re-plans and leads exactly one fresh fetch"
        );
    }

    /// A follower that keeps joining leaders which drop before publishing must eventually give up
    /// with `None` — never a fabricated `Retry` that would open the fail-closed OTEL gate on churn.
    #[tokio::test(flavor = "current_thread")]
    async fn refresh_returns_none_after_repeated_leader_drops() {
        let refresh = SettingsRefresh::default();
        let auth = distill_login::GrokAuth::test_default();

        macro_rules! new_fetch {
            () => {
                Box::pin(refresh.refresh(&auth, std::future::pending::<SettingsFetch>))
            };
        }
        macro_rules! poll_park {
            ($fut:expr, $msg:expr) => {
                tokio::select! {
                    biased;
                    _ = &mut $fut => unreachable!($msg),
                    _ = tokio::task::yield_now() => {}
                }
            };
        }

        let mut leader = new_fetch!();
        poll_park!(leader, "the first leader parks on its pending fetch");
        let mut follower = new_fetch!();
        poll_park!(follower, "the follower joins the first in-flight leader");

        for _ in 0..3 {
            let mut next = new_fetch!();
            drop(leader);
            poll_park!(next, "a fresh leader grabs the freed lead slot");
            poll_park!(
                follower,
                "the follower re-joins the fresh leader after a drop"
            );
            leader = next;
        }

        drop(leader);
        assert!(
            follower.await.is_none(),
            "past the reattempt budget the follower yields None, not a gate-opening Retry"
        );
    }
}
