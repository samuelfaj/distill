// Modified for Distill by Samuel Fajreldines, 2026.
//! Bounded deferred payloads for finish-before-spawn lifecycle delivery.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use distill_shell::extensions::notification::{SessionNotification, SessionUpdate};

use super::subagent::{SubagentAttemptKey, SubagentLifecycleState};

const MAX_DEFERRED_SUBAGENT_FINISHES: usize = 256;
const DEFERRED_SUBAGENT_FINISH_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct DeferredSubagentFinish {
    notification: SessionNotification,
    inserted_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct DeferredSubagentChild {
    lifecycle: SubagentLifecycleState,
    notifications: HashMap<SubagentAttemptKey, DeferredSubagentFinish>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DeferredSubagentFinishes {
    children: HashMap<String, DeferredSubagentChild>,
    payload_count: usize,
}

impl DeferredSubagentFinishes {
    pub(crate) fn clear(&mut self) {
        self.children.clear();
        self.payload_count = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.payload_count
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.payload_count == 0
    }

    pub(crate) fn lifecycle(
        &mut self,
        child_session_id: &str,
        now: Instant,
    ) -> Option<SubagentLifecycleState> {
        self.prune_expired(now);
        self.children
            .get(child_session_id)
            .map(|child| child.lifecycle.clone())
    }

    pub(crate) fn sanitize_lifecycle(
        &mut self,
        child_session_id: &str,
        lifecycle: &mut SubagentLifecycleState,
        now: Instant,
    ) {
        self.prune_expired(now);
        lifecycle.forget_unbacked_pending_attempts(|key| {
            self.children
                .get(child_session_id)
                .is_some_and(|child| child.notifications.contains_key(key))
        });
    }

    pub(crate) fn defer(
        &mut self,
        child_session_id: &str,
        key: SubagentAttemptKey,
        lifecycle: SubagentLifecycleState,
        mut notification: SessionNotification,
        now: Instant,
    ) {
        self.prune_expired(now);
        self.retain_lifecycle_attempts(child_session_id, &lifecycle);
        if let SessionUpdate::SubagentFinished { output, .. } = &mut notification.update {
            *output = None;
        }
        let is_replacement = self
            .children
            .get(child_session_id)
            .is_some_and(|child| child.notifications.contains_key(&key));
        if !is_replacement {
            while self.payload_count >= MAX_DEFERRED_SUBAGENT_FINISHES {
                self.evict_oldest();
            }
        }
        let child = self
            .children
            .entry(child_session_id.to_owned())
            .or_default();
        child.lifecycle = lifecycle;
        if child
            .notifications
            .insert(
                key,
                DeferredSubagentFinish {
                    notification,
                    inserted_at: now,
                },
            )
            .is_none()
        {
            self.payload_count += 1;
        }
    }

    pub(crate) fn take(
        &mut self,
        child_session_id: &str,
        key: &SubagentAttemptKey,
        now: Instant,
    ) -> Option<SessionNotification> {
        self.prune_expired(now);
        let child = self.children.get_mut(child_session_id)?;
        let payload = child.notifications.remove(key)?;
        self.payload_count = self.payload_count.saturating_sub(1);
        if child.notifications.is_empty() {
            self.children.remove(child_session_id);
        }
        Some(payload.notification)
    }

    pub(crate) fn retain_lifecycle_attempts(
        &mut self,
        child_session_id: &str,
        lifecycle: &SubagentLifecycleState,
    ) {
        let Some(child) = self.children.get_mut(child_session_id) else {
            return;
        };
        let before = child.notifications.len();
        child
            .notifications
            .retain(|key, _| lifecycle.retains_attempt(key));
        self.payload_count -= before - child.notifications.len();
        child.lifecycle = lifecycle.clone();
        if child.notifications.is_empty() {
            self.children.remove(child_session_id);
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        let mut removed = 0;
        self.children.retain(|child_session_id, child| {
            let expired: Vec<_> = child
                .notifications
                .iter()
                .filter(|(_, payload)| {
                    now.saturating_duration_since(payload.inserted_at)
                        >= DEFERRED_SUBAGENT_FINISH_TTL
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in expired {
                child.notifications.remove(&key);
                child.lifecycle.forget_pending_attempt(&key);
                removed += 1;
                tracing::debug!(
                    child_session_id,
                    attempt_id = key.attempt_id(),
                    "deferred subagent finish expired"
                );
            }
            !child.notifications.is_empty()
        });
        self.payload_count = self.payload_count.saturating_sub(removed);
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .children
            .iter()
            .flat_map(|(child_session_id, child)| {
                child.notifications.iter().map(move |(key, payload)| {
                    (child_session_id.clone(), key.clone(), payload.inserted_at)
                })
            })
            .min_by_key(|(_, _, inserted_at)| *inserted_at);
        let Some((child_session_id, key, _)) = oldest else {
            return;
        };
        let mut remove_child = false;
        if let Some(child) = self.children.get_mut(&child_session_id) {
            if child.notifications.remove(&key).is_some() {
                self.payload_count = self.payload_count.saturating_sub(1);
                child.lifecycle.forget_pending_attempt(&key);
            }
            remove_child = child.notifications.is_empty();
        }
        if remove_child {
            self.children.remove(&child_session_id);
        }
        tracing::debug!(
            child_session_id,
            attempt_id = key.attempt_id(),
            "deferred subagent finish evicted at capacity"
        );
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol as acp;

    use super::*;
    use crate::app::subagent::{SubagentLifecycleReduction, SubagentLifecycleTransition};

    fn finish_notification(child: &str, attempt_id: &str, output: &str) -> SessionNotification {
        SessionNotification {
            session_id: acp::SessionId::new("parent"),
            update: SessionUpdate::SubagentFinished {
                subagent_id: child.to_owned(),
                attempt_id: Some(attempt_id.to_owned()),
                child_session_id: child.to_owned(),
                status: "completed".to_owned(),
                error: Some(output.to_owned()),
                tool_calls: 0,
                turns: 0,
                duration_ms: 1,
                tokens_used: 0,
                output: Some(output.to_owned()),
                will_wake: false,
            },
            meta: None,
        }
    }

    fn pending_lifecycle(attempt_id: &str, event_seq: u64) -> SubagentLifecycleState {
        let SubagentLifecycleReduction::Accepted(accepted) = SubagentLifecycleState::default()
            .reduce(
                SubagentLifecycleTransition::Finished,
                Some(attempt_id),
                Some(event_seq),
            )
        else {
            panic!("finish must await spawn");
        };
        accepted.into_state()
    }

    fn deferred_payload_error(notification: &SessionNotification) -> Option<&str> {
        let SessionUpdate::SubagentFinished { error, output, .. } = &notification.update else {
            panic!("expected finish");
        };
        assert!(output.is_none(), "deferred output must be stripped");
        error.as_deref()
    }

    #[test]
    fn capacity_evicts_the_oldest_payload() {
        let mut store = DeferredSubagentFinishes::default();
        let start = Instant::now();
        for index in 0..MAX_DEFERRED_SUBAGENT_FINISHES {
            let child = format!("child-{index}");
            let attempt = format!("at1.{index}");
            store.defer(
                &child,
                SubagentAttemptKey::from_wire(Some(&attempt)),
                pending_lifecycle(&attempt, index as u64),
                finish_notification(&child, &attempt, "secret"),
                start + Duration::from_millis(index as u64),
            );
        }

        store.defer(
            "child-new",
            SubagentAttemptKey::from_wire(Some("at1.new")),
            pending_lifecycle("at1.new", 300),
            finish_notification("child-new", "at1.new", "secret"),
            start + Duration::from_secs(1),
        );

        assert_eq!(store.len(), MAX_DEFERRED_SUBAGENT_FINISHES);
        assert!(
            store
                .take(
                    "child-0",
                    &SubagentAttemptKey::from_wire(Some("at1.0")),
                    start + Duration::from_secs(2),
                )
                .is_none()
        );
        assert!(
            store
                .take(
                    "child-new",
                    &SubagentAttemptKey::from_wire(Some("at1.new")),
                    start + Duration::from_secs(2),
                )
                .is_some()
        );
    }

    #[test]
    fn attempt_history_trim_evicts_deferred_payload() {
        let mut store = DeferredSubagentFinishes::default();
        let start = Instant::now();
        let mut lifecycle = SubagentLifecycleState::default();
        for index in 0..=crate::app::subagent::SUBAGENT_ATTEMPT_HISTORY_LIMIT {
            let attempt = format!("at1.{index}");
            let SubagentLifecycleReduction::Accepted(accepted) = lifecycle.reduce(
                SubagentLifecycleTransition::Finished,
                Some(&attempt),
                Some(index as u64 + 1),
            ) else {
                panic!("finish must await spawn");
            };
            accepted.commit(&mut lifecycle);
            store.defer(
                "child",
                SubagentAttemptKey::from_wire(Some(&attempt)),
                lifecycle.clone(),
                finish_notification("child", &attempt, &attempt),
                start + Duration::from_millis(index as u64),
            );
        }

        assert_eq!(
            store.len(),
            crate::app::subagent::SUBAGENT_ATTEMPT_HISTORY_LIMIT
        );
        assert!(
            store
                .take(
                    "child",
                    &SubagentAttemptKey::from_wire(Some("at1.0")),
                    start + Duration::from_secs(1),
                )
                .is_none()
        );
    }

    #[test]
    fn insertion_prunes_expired_payloads() {
        let mut store = DeferredSubagentFinishes::default();
        let start = Instant::now();
        store.defer(
            "child-old",
            SubagentAttemptKey::from_wire(Some("at1.old")),
            pending_lifecycle("at1.old", 1),
            finish_notification("child-old", "at1.old", "secret"),
            start,
        );
        store.defer(
            "child-new",
            SubagentAttemptKey::from_wire(Some("at1.new")),
            pending_lifecycle("at1.new", 2),
            finish_notification("child-new", "at1.new", "secret"),
            start + DEFERRED_SUBAGENT_FINISH_TTL,
        );

        assert_eq!(store.len(), 1);
        assert!(
            store
                .lifecycle("child-old", start + DEFERRED_SUBAGENT_FINISH_TTL)
                .is_none()
        );
    }

    #[test]
    fn consume_rejects_an_expired_matching_payload() {
        let mut store = DeferredSubagentFinishes::default();
        let start = Instant::now();
        let key = SubagentAttemptKey::from_wire(Some("at1.old"));
        store.defer(
            "child",
            key.clone(),
            pending_lifecycle("at1.old", 1),
            finish_notification("child", "at1.old", "secret"),
            start,
        );

        assert!(
            store
                .take("child", &key, start + DEFERRED_SUBAGENT_FINISH_TTL,)
                .is_none()
        );
        assert!(store.is_empty());
    }

    #[test]
    fn consume_selects_only_the_matching_attempt_payload() {
        let mut store = DeferredSubagentFinishes::default();
        let start = Instant::now();
        let mut lifecycle = pending_lifecycle("at1.one", 1);
        let SubagentLifecycleReduction::Accepted(accepted) = lifecycle.reduce(
            SubagentLifecycleTransition::Finished,
            Some("at1.two"),
            Some(2),
        ) else {
            panic!("second finish must await spawn");
        };
        accepted.commit(&mut lifecycle);
        store.defer(
            "child",
            SubagentAttemptKey::from_wire(Some("at1.one")),
            lifecycle.clone(),
            finish_notification("child", "at1.one", "first"),
            start,
        );
        store.defer(
            "child",
            SubagentAttemptKey::from_wire(Some("at1.two")),
            lifecycle,
            finish_notification("child", "at1.two", "second"),
            start + Duration::from_millis(1),
        );

        let second = store
            .take(
                "child",
                &SubagentAttemptKey::from_wire(Some("at1.two")),
                start + Duration::from_secs(1),
            )
            .expect("matching payload");
        assert_eq!(deferred_payload_error(&second), Some("second"));
        assert_eq!(store.len(), 1);
        assert!(
            store
                .take(
                    "child",
                    &SubagentAttemptKey::from_wire(Some("at1.one")),
                    start + Duration::from_secs(1),
                )
                .is_some()
        );
    }
}
