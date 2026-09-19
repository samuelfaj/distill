//! Attempt-aware subagent lifecycle reducer.
//!
//! Lifecycle is keyed by `(subagent_id, attempt_id)`; missing attempt IDs share the legacy key.
//! Spawn makes an attempt current, progress applies only to the current running attempt, and finish
//! either closes a known attempt or waits for its spawn. Reducer effects are domain-neutral: apply
//! the transition, replace the current attempt, apply with a pending finish, retain a finish until
//! spawn, or record a non-current terminal transition.
//!
//! Sequence numbers are per-attempt high-water marks, while `last_spawn_seq` rejects stale spawn
//! replay across evicted attempts. Missing sequences remain compatible for retained legacy events,
//! but cannot introduce an unseen attempt after sequenced spawn history. A finished unsequenced
//! legacy spawn is a duplicate; only a newer sequence proves a legacy restart.
//!
//! The current attempt is never evicted. At most eight attempts are retained; an evicted key becomes
//! unseen, so stale-spawn guards apply again. `PendingFinish` has meaning only while the bounded,
//! expiring payload in `DeferredSubagentFinishes` exists; its owner removes unbacked pending state.

use std::collections::{HashMap, VecDeque};

/// A child retains only its eight most recently observed attempts. Replays for
/// attempts outside this window are treated as unseen after eviction.
pub(crate) const SUBAGENT_ATTEMPT_HISTORY_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentLifecyclePhase {
    Running,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentLifecycleTransition {
    Spawned,
    Progress,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum SubagentAttemptKey {
    Legacy,
    Id(String),
}

impl SubagentAttemptKey {
    pub(crate) fn from_wire(attempt_id: Option<&str>) -> Self {
        attempt_id.map_or(Self::Legacy, |id| Self::Id(id.to_owned()))
    }

    pub(crate) fn attempt_id(&self) -> Option<&str> {
        match self {
            Self::Legacy => None,
            Self::Id(id) => Some(id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptPhase {
    PendingFinish,
    Running,
    Finished,
}

#[derive(Debug, Clone)]
struct AttemptLifecycle {
    phase: AttemptPhase,
    last_event_seq: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentLifecycleState {
    current: Option<SubagentAttemptKey>,
    attempts: HashMap<SubagentAttemptKey, AttemptLifecycle>,
    order: VecDeque<SubagentAttemptKey>,
    last_spawn_seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentLifecycleEffect {
    /// Render this transition for the current attempt.
    Apply,
    /// Render an unseen attempt and retire the prior current attempt, even if it was running.
    ApplyNewAttempt,
    /// Render an unseen attempt, then render its previously deferred finish.
    ApplyWithPendingFinish,
    /// Retain this finish until the matching spawn arrives.
    AwaitSpawn,
    /// Commit a transition for a retained non-current attempt without rendering it.
    RecordOnly,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedSubagentLifecycle {
    next: SubagentLifecycleState,
    effect: SubagentLifecycleEffect,
}

#[derive(Debug, Clone)]
pub(crate) enum SubagentLifecycleReduction {
    Accepted(AcceptedSubagentLifecycle),
    Dropped,
}

impl AcceptedSubagentLifecycle {
    pub(crate) fn effect(&self) -> SubagentLifecycleEffect {
        self.effect
    }

    pub(crate) fn commit(self, state: &mut SubagentLifecycleState) {
        *state = self.next;
    }

    pub(crate) fn into_state(self) -> SubagentLifecycleState {
        self.next
    }
}

impl SubagentLifecycleState {
    #[cfg(test)]
    pub(crate) fn running_legacy_for_test() -> Self {
        let accepted =
            match Self::default().reduce(SubagentLifecycleTransition::Spawned, None, None) {
                SubagentLifecycleReduction::Accepted(accepted) => accepted,
                SubagentLifecycleReduction::Dropped => unreachable!(),
            };
        accepted.next
    }

    #[cfg(test)]
    pub(crate) fn set_finished_for_test(&mut self, finished: bool) {
        let key = self.current.clone().unwrap_or(SubagentAttemptKey::Legacy);
        self.current = Some(key.clone());
        self.attempts.insert(
            key.clone(),
            AttemptLifecycle {
                phase: if finished {
                    AttemptPhase::Finished
                } else {
                    AttemptPhase::Running
                },
                last_event_seq: None,
            },
        );
        touch_attempt(&mut self.order, key);
    }

    /// Missing attempt ids share one legacy key. Once attempt-aware history exists,
    /// an unsequenced unseen non-legacy spawn is rejected because an evicted replay
    /// cannot be distinguished from a new attempt. An unseen sequenced attempt becomes
    /// current and retires the prior attempt from presentation. A matching pending
    /// finish, or a legacy pending finish for a typed spawn, is consumed into `Finished`.
    pub(crate) fn reduce(
        &self,
        transition: SubagentLifecycleTransition,
        attempt_id: Option<&str>,
        event_seq: Option<u64>,
    ) -> SubagentLifecycleReduction {
        let key = SubagentAttemptKey::from_wire(attempt_id);
        match transition {
            SubagentLifecycleTransition::Spawned => self.reduce_spawn(key, event_seq),
            SubagentLifecycleTransition::Progress => self.reduce_progress(&key, event_seq),
            SubagentLifecycleTransition::Finished => self.reduce_finish(key, event_seq),
        }
    }

    pub(crate) fn current_attempt_key(&self) -> Option<&SubagentAttemptKey> {
        self.current.as_ref()
    }

    pub(crate) fn current_attempt_id(&self) -> Option<&str> {
        self.current_attempt_key()
            .and_then(SubagentAttemptKey::attempt_id)
    }

    pub(crate) fn phase(&self) -> Option<SubagentLifecyclePhase> {
        self.current.as_ref().and_then(|key| {
            self.attempts
                .get(key)
                .and_then(|attempt| match attempt.phase {
                    AttemptPhase::Running => Some(SubagentLifecyclePhase::Running),
                    AttemptPhase::Finished => Some(SubagentLifecyclePhase::Finished),
                    AttemptPhase::PendingFinish => None,
                })
        })
    }

    #[cfg(test)]
    pub(crate) fn last_event_seq(&self) -> Option<u64> {
        self.current
            .as_ref()
            .and_then(|key| self.attempts.get(key))
            .and_then(|attempt| attempt.last_event_seq)
    }

    #[cfg(test)]
    pub(crate) fn is_attempt_finished(&self, attempt_id: &str) -> bool {
        self.attempts
            .get(&SubagentAttemptKey::Id(attempt_id.to_owned()))
            .is_some_and(|attempt| attempt.phase == AttemptPhase::Finished)
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.phase() == Some(SubagentLifecyclePhase::Finished)
    }

    pub(crate) fn has_current_attempt(&self) -> bool {
        self.current.is_some()
    }

    pub(crate) fn retains_attempt(&self, key: &SubagentAttemptKey) -> bool {
        self.attempts.contains_key(key)
    }

    pub(crate) fn forget_pending_attempt(&mut self, key: &SubagentAttemptKey) {
        if self
            .attempts
            .get(key)
            .is_some_and(|attempt| attempt.phase == AttemptPhase::PendingFinish)
        {
            self.attempts.remove(key);
            self.order.retain(|candidate| candidate != key);
        }
    }

    pub(crate) fn forget_unbacked_pending_attempts(
        &mut self,
        mut is_backed: impl FnMut(&SubagentAttemptKey) -> bool,
    ) {
        let orphaned: Vec<_> = self
            .attempts
            .iter()
            .filter(|(key, attempt)| {
                attempt.phase == AttemptPhase::PendingFinish && !is_backed(key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in orphaned {
            self.forget_pending_attempt(&key);
        }
    }

    fn reduce_spawn(
        &self,
        key: SubagentAttemptKey,
        event_seq: Option<u64>,
    ) -> SubagentLifecycleReduction {
        // Only a retained legacy attempt may still take unsequenced events; an unseen legacy key
        // after sequenced history is an evicted replay, not a new attempt.
        if self.last_spawn_seq.is_some()
            && event_seq.is_none()
            && (key != SubagentAttemptKey::Legacy || !self.attempts.contains_key(&key))
        {
            return SubagentLifecycleReduction::Dropped;
        }
        if event_seq
            .zip(self.last_spawn_seq)
            .is_some_and(|(incoming, high_water)| incoming <= high_water)
        {
            return SubagentLifecycleReduction::Dropped;
        }
        if let Some(existing) = self.attempts.get(&key) {
            if key == SubagentAttemptKey::Legacy
                && self.current.as_ref() == Some(&key)
                && existing.phase == AttemptPhase::Finished
                && event_seq.is_some()
                && is_newer(event_seq, existing.last_event_seq)
            {
                let mut next = self.clone();
                next.attempts.insert(
                    key,
                    AttemptLifecycle {
                        phase: AttemptPhase::Running,
                        last_event_seq: max_seq(existing.last_event_seq, event_seq),
                    },
                );
                next.last_spawn_seq = max_seq(next.last_spawn_seq, event_seq);
                return accepted(next, SubagentLifecycleEffect::ApplyNewAttempt);
            }
            if existing.phase == AttemptPhase::PendingFinish {
                let mut next = self.clone();
                if matches!(key, SubagentAttemptKey::Id(_)) {
                    next.attempts.remove(&SubagentAttemptKey::Legacy);
                    next.order
                        .retain(|candidate| candidate != &SubagentAttemptKey::Legacy);
                }
                next.current = Some(key.clone());
                next.attempts.insert(
                    key.clone(),
                    AttemptLifecycle {
                        phase: AttemptPhase::Finished,
                        last_event_seq: max_seq(existing.last_event_seq, event_seq),
                    },
                );
                touch_attempt(&mut next.order, key);
                next.last_spawn_seq = max_seq(next.last_spawn_seq, event_seq);
                return accepted(next, SubagentLifecycleEffect::ApplyWithPendingFinish);
            }
            return SubagentLifecycleReduction::Dropped;
        }

        let mut next = self.clone();
        let is_new_attempt = next.current.is_some();
        next.current = Some(key.clone());
        let legacy_pending = matches!(key, SubagentAttemptKey::Id(_))
            .then(|| next.attempts.get(&SubagentAttemptKey::Legacy))
            .flatten()
            .filter(|attempt| attempt.phase == AttemptPhase::PendingFinish)
            .cloned();
        if let Some(pending) = legacy_pending {
            next.attempts.remove(&SubagentAttemptKey::Legacy);
            next.order
                .retain(|candidate| candidate != &SubagentAttemptKey::Legacy);
            next.attempts.insert(
                key.clone(),
                AttemptLifecycle {
                    phase: AttemptPhase::Finished,
                    last_event_seq: max_seq(pending.last_event_seq, event_seq),
                },
            );
            touch_attempt(&mut next.order, key);
            next.last_spawn_seq = max_seq(next.last_spawn_seq, event_seq);
            trim_attempts(&mut next);
            return accepted(next, SubagentLifecycleEffect::ApplyWithPendingFinish);
        }
        next.attempts.insert(
            key.clone(),
            AttemptLifecycle {
                phase: AttemptPhase::Running,
                last_event_seq: event_seq,
            },
        );
        touch_attempt(&mut next.order, key);
        next.last_spawn_seq = max_seq(next.last_spawn_seq, event_seq);
        trim_attempts(&mut next);
        accepted(
            next,
            if is_new_attempt {
                SubagentLifecycleEffect::ApplyNewAttempt
            } else {
                SubagentLifecycleEffect::Apply
            },
        )
    }

    fn reduce_progress(
        &self,
        key: &SubagentAttemptKey,
        event_seq: Option<u64>,
    ) -> SubagentLifecycleReduction {
        let Some(attempt) = self.attempts.get(key) else {
            // Progress is transient and the terminal update carries final counts, so buffering it
            // would only let an out-of-order attempt overwrite current presentation.
            return SubagentLifecycleReduction::Dropped;
        };
        if self.current.as_ref() != Some(key)
            || attempt.phase != AttemptPhase::Running
            || !is_newer(event_seq, attempt.last_event_seq)
        {
            return SubagentLifecycleReduction::Dropped;
        }
        let mut next = self.clone();
        if let Some(attempt) = next.attempts.get_mut(key) {
            attempt.last_event_seq = max_seq(attempt.last_event_seq, event_seq);
        }
        accepted(next, SubagentLifecycleEffect::Apply)
    }

    fn reduce_finish(
        &self,
        mut key: SubagentAttemptKey,
        event_seq: Option<u64>,
    ) -> SubagentLifecycleReduction {
        if key == SubagentAttemptKey::Legacy
            && let Some(typed_key @ SubagentAttemptKey::Id(_)) = self.current.as_ref()
        {
            let Some(attempt) = self.attempts.get(typed_key) else {
                return SubagentLifecycleReduction::Dropped;
            };
            if attempt.phase != AttemptPhase::Running {
                return SubagentLifecycleReduction::Dropped;
            }
            key = typed_key.clone();
        }
        if let Some(attempt) = self.attempts.get(&key) {
            if attempt.phase != AttemptPhase::Running
                || !is_newer(event_seq, attempt.last_event_seq)
            {
                return SubagentLifecycleReduction::Dropped;
            }
            let mut next = self.clone();
            if let Some(attempt) = next.attempts.get_mut(&key) {
                attempt.phase = AttemptPhase::Finished;
                attempt.last_event_seq = max_seq(attempt.last_event_seq, event_seq);
            }
            return if next.current.as_ref() == Some(&key) {
                accepted(next, SubagentLifecycleEffect::Apply)
            } else {
                accepted(next, SubagentLifecycleEffect::RecordOnly)
            };
        }
        if event_seq
            .zip(self.last_spawn_seq)
            .is_some_and(|(incoming, high_water)| incoming <= high_water)
        {
            return SubagentLifecycleReduction::Dropped;
        }
        let mut next = self.clone();
        next.attempts.insert(
            key.clone(),
            AttemptLifecycle {
                phase: AttemptPhase::PendingFinish,
                last_event_seq: event_seq,
            },
        );
        touch_attempt(&mut next.order, key);
        trim_attempts(&mut next);
        accepted(next, SubagentLifecycleEffect::AwaitSpawn)
    }
}

fn accepted(
    next: SubagentLifecycleState,
    effect: SubagentLifecycleEffect,
) -> SubagentLifecycleReduction {
    SubagentLifecycleReduction::Accepted(AcceptedSubagentLifecycle { next, effect })
}

fn is_newer(incoming: Option<u64>, current: Option<u64>) -> bool {
    incoming
        .zip(current)
        .is_none_or(|(incoming, current)| incoming > current)
}

fn max_seq(current: Option<u64>, incoming: Option<u64>) -> Option<u64> {
    match (current, incoming) {
        (Some(current), Some(incoming)) => Some(current.max(incoming)),
        (current, incoming) => current.or(incoming),
    }
}

fn touch_attempt(order: &mut VecDeque<SubagentAttemptKey>, key: SubagentAttemptKey) {
    if let Some(index) = order.iter().position(|existing| existing == &key) {
        order.remove(index);
    }
    order.push_back(key);
}

/// The current attempt counts toward the limit but is never evicted.
fn trim_attempts(state: &mut SubagentLifecycleState) {
    while state.attempts.len() > SUBAGENT_ATTEMPT_HISTORY_LIMIT {
        let Some(index) = state
            .order
            .iter()
            .position(|key| state.current.as_ref() != Some(key))
        else {
            break;
        };
        if let Some(oldest_retired) = state.order.remove(index) {
            state.attempts.remove(&oldest_retired);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Step = (
        SubagentLifecycleTransition,
        Option<&'static str>,
        Option<u64>,
    );

    fn apply(state: &mut SubagentLifecycleState, step: Step) -> SubagentLifecycleEffect {
        let SubagentLifecycleReduction::Accepted(accepted) = state.reduce(step.0, step.1, step.2)
        else {
            panic!("transition dropped");
        };
        let effect = accepted.effect();
        accepted.commit(state);
        effect
    }

    #[test]
    fn transition_matrix() {
        struct Case {
            setup: &'static [Step],
            step: Step,
            effect: Option<SubagentLifecycleEffect>,
            current: Option<&'static str>,
            is_finished: bool,
        }
        use SubagentLifecycleEffect::{ApplyNewAttempt, ApplyWithPendingFinish};
        use SubagentLifecycleTransition::{Finished, Progress, Spawned};
        let cases = [
            Case {
                setup: &[(Spawned, Some("at1.a"), Some(1))],
                step: (Spawned, Some("at1.b"), Some(2)),
                effect: Some(ApplyNewAttempt),
                current: Some("at1.b"),
                is_finished: false,
            },
            Case {
                setup: &[
                    (Spawned, Some("at1.a"), Some(1)),
                    (Finished, Some("at1.a"), Some(100)),
                ],
                step: (Spawned, Some("at1.b"), Some(3)),
                effect: Some(ApplyNewAttempt),
                current: Some("at1.b"),
                is_finished: false,
            },
            Case {
                setup: &[(Spawned, Some("at1.a"), Some(1))],
                step: (Finished, None, Some(2)),
                effect: Some(SubagentLifecycleEffect::Apply),
                current: Some("at1.a"),
                is_finished: true,
            },
            Case {
                setup: &[(Finished, Some("at1.a"), Some(2))],
                step: (Spawned, Some("at1.a"), Some(1)),
                effect: Some(ApplyWithPendingFinish),
                current: Some("at1.a"),
                is_finished: true,
            },
            Case {
                setup: &[
                    (Spawned, Some("at1.a"), Some(1)),
                    (Finished, Some("at1.a"), Some(2)),
                    (Finished, Some("at1.b"), Some(4)),
                ],
                step: (Spawned, Some("at1.b"), Some(3)),
                effect: Some(ApplyWithPendingFinish),
                current: Some("at1.b"),
                is_finished: true,
            },
            Case {
                setup: &[
                    (Spawned, Some("at1.a"), Some(1)),
                    (Spawned, Some("at1.b"), Some(2)),
                ],
                step: (Progress, Some("at1.a"), Some(3)),
                effect: None,
                current: Some("at1.b"),
                is_finished: false,
            },
            Case {
                setup: &[(Spawned, None, None), (Finished, None, None)],
                step: (Spawned, None, None),
                effect: None,
                current: None,
                is_finished: true,
            },
            Case {
                setup: &[(Spawned, None, Some(1)), (Finished, None, Some(2))],
                step: (Spawned, None, Some(3)),
                effect: Some(ApplyNewAttempt),
                current: None,
                is_finished: false,
            },
        ];

        for case in cases {
            let mut state = SubagentLifecycleState::default();
            for &step in case.setup {
                apply(&mut state, step);
            }
            match (
                state.reduce(case.step.0, case.step.1, case.step.2),
                case.effect,
            ) {
                (SubagentLifecycleReduction::Accepted(accepted), Some(effect)) => {
                    assert_eq!(accepted.effect(), effect);
                    accepted.commit(&mut state);
                }
                (SubagentLifecycleReduction::Dropped, None) => {}
                _ => panic!("unexpected reduction"),
            }
            assert_eq!(state.current_attempt_id(), case.current);
            assert_eq!(state.is_finished(), case.is_finished);
        }
    }

    #[test]
    fn retained_prior_attempt_finish_records_below_spawn_high_water() {
        use SubagentLifecycleTransition::{Finished, Spawned};

        let mut state = SubagentLifecycleState::default();
        apply(&mut state, (Spawned, Some("at1.a"), Some(1)));
        apply(&mut state, (Spawned, Some("at1.b"), Some(3)));

        assert_eq!(
            apply(&mut state, (Finished, Some("at1.a"), Some(2))),
            SubagentLifecycleEffect::RecordOnly
        );
        assert!(state.is_attempt_finished("at1.a"));
        assert_eq!(state.current_attempt_id(), Some("at1.b"));
        assert!(!state.is_finished());
    }

    #[test]
    fn evicted_prior_attempt_finish_below_spawn_high_water_is_dropped() {
        use SubagentLifecycleTransition::{Finished, Spawned};

        let mut state = SubagentLifecycleState::default();
        for (index, attempt_id) in [
            "at1.0", "at1.1", "at1.2", "at1.3", "at1.4", "at1.5", "at1.6", "at1.7", "at1.8",
        ]
        .into_iter()
        .enumerate()
        {
            apply(
                &mut state,
                (Spawned, Some(attempt_id), Some(index as u64 + 1)),
            );
        }
        assert!(!state.retains_attempt(&SubagentAttemptKey::Id("at1.0".to_owned())));
        assert!(matches!(
            state.reduce(Finished, Some("at1.0"), Some(2)),
            SubagentLifecycleReduction::Dropped
        ));
        assert_eq!(state.current_attempt_id(), Some("at1.8"));
    }

    #[test]
    fn typed_spawn_consumes_pending_legacy_finish() {
        use SubagentLifecycleTransition::{Finished, Spawned};

        let mut state = SubagentLifecycleState::default();
        apply(&mut state, (Finished, None, Some(2)));

        assert_eq!(
            apply(&mut state, (Spawned, Some("at1.a"), Some(1))),
            SubagentLifecycleEffect::ApplyWithPendingFinish
        );
        assert_eq!(state.current_attempt_id(), Some("at1.a"));
        assert!(state.is_finished());
        assert!(!state.retains_attempt(&SubagentAttemptKey::Legacy));
    }

    #[test]
    fn exact_pending_finish_discards_sibling_legacy_finish() {
        use SubagentLifecycleTransition::{Finished, Spawned};

        let mut state = SubagentLifecycleState::default();
        apply(&mut state, (Finished, None, Some(2)));
        apply(&mut state, (Finished, Some("at1.a"), Some(3)));

        assert_eq!(
            apply(&mut state, (Spawned, Some("at1.a"), Some(1))),
            SubagentLifecycleEffect::ApplyWithPendingFinish
        );
        assert!(state.is_attempt_finished("at1.a"));
        assert!(!state.retains_attempt(&SubagentAttemptKey::Legacy));
        assert_eq!(
            apply(&mut state, (Spawned, Some("at1.b"), Some(4))),
            SubagentLifecycleEffect::ApplyNewAttempt
        );
        assert_eq!(state.current_attempt_id(), Some("at1.b"));
        assert!(!state.is_finished());
    }

    #[test]
    fn pending_legacy_finish_closes_only_the_first_typed_spawn() {
        use SubagentLifecycleTransition::{Finished, Spawned};

        let mut state = SubagentLifecycleState::default();
        apply(&mut state, (Finished, None, Some(2)));
        apply(&mut state, (Spawned, Some("at1.a"), Some(1)));

        assert_eq!(
            apply(&mut state, (Spawned, Some("at1.b"), Some(3))),
            SubagentLifecycleEffect::ApplyNewAttempt
        );
        assert!(state.is_attempt_finished("at1.a"));
        assert_eq!(state.current_attempt_id(), Some("at1.b"));
        assert!(!state.is_finished());
    }

    #[test]
    fn legacy_finish_closes_current_typed_attempt_after_wake() {
        use SubagentLifecycleTransition::{Finished, Spawned};

        let mut state = SubagentLifecycleState::default();
        apply(&mut state, (Spawned, Some("at1.a"), Some(1)));
        apply(&mut state, (Finished, Some("at1.a"), Some(2)));
        apply(&mut state, (Spawned, Some("at1.b"), Some(3)));

        assert_eq!(
            apply(&mut state, (Finished, None, Some(4))),
            SubagentLifecycleEffect::Apply
        );
        assert_eq!(state.current_attempt_id(), Some("at1.b"));
        assert!(state.is_finished());
    }

    #[test]
    fn legacy_finish_after_typed_completion_is_dropped() {
        use SubagentLifecycleTransition::{Finished, Spawned};

        let mut state = SubagentLifecycleState::default();
        apply(&mut state, (Spawned, Some("at1.a"), Some(1)));
        apply(&mut state, (Finished, Some("at1.a"), Some(2)));

        assert!(matches!(
            state.reduce(Finished, None, Some(3)),
            SubagentLifecycleReduction::Dropped
        ));
        assert_eq!(state.last_event_seq(), Some(2));
        assert!(!state.retains_attempt(&SubagentAttemptKey::Legacy));
    }

    #[test]
    fn evicted_attempt_spawns_are_rejected() {
        let mut state = SubagentLifecycleState::default();
        for (index, attempt_id) in [
            "at1.0", "at1.1", "at1.2", "at1.3", "at1.4", "at1.5", "at1.6", "at1.7", "at1.8",
        ]
        .into_iter()
        .enumerate()
        {
            apply(
                &mut state,
                (
                    SubagentLifecycleTransition::Spawned,
                    Some(attempt_id),
                    Some(index as u64 + 1),
                ),
            );
        }
        for event_seq in [Some(1), None] {
            assert!(matches!(
                state.reduce(
                    SubagentLifecycleTransition::Spawned,
                    Some("at1.0"),
                    event_seq
                ),
                SubagentLifecycleReduction::Dropped
            ));
        }
        assert_eq!(state.current_attempt_id(), Some("at1.8"));
    }

    #[test]
    fn unseen_unsequenced_legacy_spawn_after_sequenced_history_is_rejected() {
        // Never-seen legacy key after a sequenced typed spawn.
        let mut state = SubagentLifecycleState::default();
        apply(
            &mut state,
            (SubagentLifecycleTransition::Spawned, Some("at1.a"), Some(1)),
        );
        assert!(matches!(
            state.reduce(SubagentLifecycleTransition::Spawned, None, None),
            SubagentLifecycleReduction::Dropped
        ));
        assert_eq!(state.current_attempt_id(), Some("at1.a"));

        // Legacy key evicted by the attempt window, then replayed without a sequence.
        let mut state = SubagentLifecycleState::default();
        apply(
            &mut state,
            (SubagentLifecycleTransition::Spawned, None, Some(1)),
        );
        for (index, attempt_id) in [
            "at1.0", "at1.1", "at1.2", "at1.3", "at1.4", "at1.5", "at1.6", "at1.7",
        ]
        .into_iter()
        .enumerate()
        {
            apply(
                &mut state,
                (
                    SubagentLifecycleTransition::Spawned,
                    Some(attempt_id),
                    Some(index as u64 + 2),
                ),
            );
        }
        assert!(!state.retains_attempt(&SubagentAttemptKey::Legacy));
        assert!(matches!(
            state.reduce(SubagentLifecycleTransition::Spawned, None, None),
            SubagentLifecycleReduction::Dropped
        ));
        assert_eq!(state.current_attempt_id(), Some("at1.7"));

        // A retained legacy pending finish still admits its unsequenced spawn.
        let mut state = SubagentLifecycleState::default();
        assert_eq!(
            apply(
                &mut state,
                (SubagentLifecycleTransition::Finished, None, None)
            ),
            SubagentLifecycleEffect::AwaitSpawn
        );
        assert_eq!(
            apply(
                &mut state,
                (SubagentLifecycleTransition::Spawned, None, None)
            ),
            SubagentLifecycleEffect::ApplyWithPendingFinish
        );
    }
}
