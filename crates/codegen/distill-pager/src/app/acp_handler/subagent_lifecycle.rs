use super::*;
use crate::app::subagent::{
    AcceptedSubagentLifecycle, SubagentAttemptKey, SubagentLifecycleEffect,
    SubagentLifecycleReduction, SubagentLifecycleState, SubagentLifecycleTransition,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifecycleOrigin {
    Stream,
    Reconciliation,
}

pub(super) struct SubagentLifecycleUpdate<'a> {
    child_session_id: &'a str,
    attempt_id: Option<&'a str>,
    transition: SubagentLifecycleTransition,
    origin: LifecycleOrigin,
}

pub(super) struct TuiSubagentLifecycleAction {
    child_session_id: String,
    attempt_key: SubagentAttemptKey,
    accepted: Option<AcceptedSubagentLifecycle>,
    should_render: bool,
    is_new_attempt: bool,
    deferred_finish: Option<SessionNotification>,
    deferred_notification: Option<SessionNotification>,
    record_only_tokens: Option<u64>,
    retired_attempt_key: Option<SubagentAttemptKey>,
    retired_started_entry_id: Option<crate::scrollback::entry::EntryId>,
    had_current_attempt: bool,
}

pub(super) struct CommittedTuiSubagentLifecycle {
    pub(super) lifecycle: SubagentLifecycleState,
    pub(super) pending_finish: Option<SessionNotification>,
    pub(super) is_new_attempt: bool,
    pub(super) is_wake: bool,
    pub(super) rebuilt_current_attempt: bool,
}

impl TuiSubagentLifecycleAction {
    pub(super) fn should_render(&self) -> bool {
        self.should_render
    }

    pub(super) fn commit(
        self,
        agent: &mut AgentView,
        now: std::time::Instant,
    ) -> Option<CommittedTuiSubagentLifecycle> {
        let rebuilt_current_attempt = self.accepted.is_none();
        let lifecycle = if let Some(accepted) = self.accepted {
            accepted.into_state()
        } else {
            agent
                .subagent_sessions
                .get(&self.child_session_id)
                .map(|info| info.attempt.lifecycle.clone())
                .unwrap_or_default()
        };

        if let Some(info) = agent.subagent_sessions.get_mut(&self.child_session_id) {
            info.attempt.lifecycle = lifecycle.clone();
            if let Some(tokens) = self.record_only_tokens {
                info.seal_attempt_tokens(self.attempt_key.clone(), tokens);
            }
            if let Some(attempt_key) = self.retired_attempt_key {
                let tokens = info.attempt.tokens_used.unwrap_or(0);
                info.seal_attempt_tokens(attempt_key, tokens);
            }
        }
        if let Some(entry_id) = self.retired_started_entry_id {
            agent.scrollback.finish_running(entry_id);
        }
        agent
            .deferred_subagent_finishes
            .retain_lifecycle_attempts(&self.child_session_id, &lifecycle);
        if let Some(notification) = self.deferred_notification {
            agent.deferred_subagent_finishes.defer(
                &self.child_session_id,
                self.attempt_key,
                lifecycle,
                notification,
                now,
            );
            return None;
        }

        if !self.should_render {
            return None;
        }
        Some(CommittedTuiSubagentLifecycle {
            is_new_attempt: self.is_new_attempt,
            is_wake: self.is_new_attempt
                && self.attempt_key.attempt_id().is_some()
                && self.had_current_attempt,
            lifecycle,
            pending_finish: self.deferred_finish,
            rebuilt_current_attempt,
        })
    }
}

pub(super) fn classify_subagent_lifecycle(
    update: &XaiSessionUpdate,
    origin: LifecycleOrigin,
) -> Option<SubagentLifecycleUpdate<'_>> {
    match update {
        XaiSessionUpdate::SubagentSpawned {
            child_session_id,
            attempt_id,
            ..
        } => Some(SubagentLifecycleUpdate {
            child_session_id,
            attempt_id: attempt_id.as_deref(),
            transition: SubagentLifecycleTransition::Spawned,
            origin,
        }),
        XaiSessionUpdate::SubagentProgress {
            child_session_id,
            attempt_id,
            ..
        } => Some(SubagentLifecycleUpdate {
            child_session_id,
            attempt_id: attempt_id.as_deref(),
            transition: SubagentLifecycleTransition::Progress,
            origin,
        }),
        XaiSessionUpdate::SubagentFinished {
            child_session_id,
            attempt_id,
            ..
        } => Some(SubagentLifecycleUpdate {
            child_session_id,
            attempt_id: attempt_id.as_deref(),
            transition: SubagentLifecycleTransition::Finished,
            origin,
        }),
        _ => None,
    }
}

pub(super) fn prepare_tui_subagent_lifecycle(
    agent: &mut AgentView,
    lifecycle: &SubagentLifecycleUpdate<'_>,
    is_replay: bool,
    event_seq: Option<u64>,
    notification: &SessionNotification,
    now: std::time::Instant,
) -> Option<TuiSubagentLifecycleAction> {
    let attempt_key = SubagentAttemptKey::from_wire(lifecycle.attempt_id);
    let current_attempt_key = agent
        .subagent_sessions
        .get(lifecycle.child_session_id)
        .and_then(|info| info.attempt.lifecycle.current_attempt_key());
    let is_current_replay_attempt = current_attempt_key == Some(&attempt_key);
    if is_replay
        && is_current_replay_attempt
        && lifecycle.transition == SubagentLifecycleTransition::Spawned
        && agent
            .subagent_sessions
            .get(lifecycle.child_session_id)
            .is_some_and(|info| {
                info.attempt.workflow_run_id.is_none()
                    && info
                        .attempt
                        .scrollback_entry_id
                        .is_none_or(|id| agent.scrollback.get_by_id(id).is_none())
            })
    {
        return Some(TuiSubagentLifecycleAction {
            child_session_id: lifecycle.child_session_id.to_owned(),
            attempt_key,
            accepted: None,
            should_render: true,
            is_new_attempt: false,
            deferred_finish: None,
            deferred_notification: None,
            record_only_tokens: None,
            retired_attempt_key: None,
            retired_started_entry_id: None,
            had_current_attempt: true,
        });
    }
    if lifecycle.origin == LifecycleOrigin::Reconciliation
        && lifecycle.transition == SubagentLifecycleTransition::Finished
    {
        let current = agent
            .subagent_sessions
            .get(lifecycle.child_session_id)
            .map(|info| info.attempt.lifecycle.clone())
            .unwrap_or_default();
        let accepted = match current.reduce(lifecycle.transition, lifecycle.attempt_id, event_seq) {
            SubagentLifecycleReduction::Accepted(accepted) => Some(accepted),
            SubagentLifecycleReduction::Dropped => None,
        };
        return Some(TuiSubagentLifecycleAction {
            child_session_id: lifecycle.child_session_id.to_owned(),
            attempt_key,
            accepted,
            should_render: true,
            is_new_attempt: false,
            deferred_finish: None,
            deferred_notification: None,
            record_only_tokens: None,
            retired_attempt_key: None,
            retired_started_entry_id: None,
            had_current_attempt: true,
        });
    }

    let mut current = agent
        .subagent_sessions
        .get(lifecycle.child_session_id)
        .map(|info| info.attempt.lifecycle.clone())
        .or_else(|| {
            agent
                .deferred_subagent_finishes
                .lifecycle(lifecycle.child_session_id, now)
        })
        .unwrap_or_default();
    agent.deferred_subagent_finishes.sanitize_lifecycle(
        lifecycle.child_session_id,
        &mut current,
        now,
    );
    let had_current_attempt = current.has_current_attempt();
    let SubagentLifecycleReduction::Accepted(accepted) =
        current.reduce(lifecycle.transition, lifecycle.attempt_id, event_seq)
    else {
        return None;
    };
    let effect = accepted.effect();
    let retires_current = matches!(
        effect,
        SubagentLifecycleEffect::ApplyNewAttempt | SubagentLifecycleEffect::ApplyWithPendingFinish
    );
    let retired_attempt_key = retires_current
        .then(|| current.current_attempt_key().cloned())
        .flatten();
    let retired_started_entry_id = retires_current
        .then(|| {
            agent
                .subagent_sessions
                .get(lifecycle.child_session_id)
                .and_then(|info| info.attempt.scrollback_entry_id)
        })
        .flatten();
    let should_render = matches!(
        effect,
        SubagentLifecycleEffect::Apply
            | SubagentLifecycleEffect::ApplyNewAttempt
            | SubagentLifecycleEffect::ApplyWithPendingFinish
    );
    let is_new_attempt = matches!(
        effect,
        SubagentLifecycleEffect::ApplyNewAttempt | SubagentLifecycleEffect::ApplyWithPendingFinish
    );
    let deferred_finish = (effect == SubagentLifecycleEffect::ApplyWithPendingFinish)
        .then(|| {
            let exact = agent.deferred_subagent_finishes.take(
                lifecycle.child_session_id,
                &attempt_key,
                now,
            );
            let legacy = attempt_key.attempt_id().is_some().then(|| {
                agent.deferred_subagent_finishes.take(
                    lifecycle.child_session_id,
                    &SubagentAttemptKey::Legacy,
                    now,
                )
            });
            exact.or_else(|| legacy.flatten())
        })
        .flatten();
    let record_only_tokens = (effect == SubagentLifecycleEffect::RecordOnly)
        .then_some(match &notification.update {
            XaiSessionUpdate::SubagentFinished { tokens_used, .. } => Some(*tokens_used),
            _ => None,
        })
        .flatten();
    let deferred_notification = (effect == SubagentLifecycleEffect::AwaitSpawn).then(|| {
        let mut notification = notification.clone();
        if let XaiSessionUpdate::SubagentFinished { output, .. } = &mut notification.update {
            *output = None;
        }
        notification
    });

    Some(TuiSubagentLifecycleAction {
        child_session_id: lifecycle.child_session_id.to_owned(),
        attempt_key,
        accepted: Some(accepted),
        should_render,
        is_new_attempt,
        deferred_finish,
        deferred_notification,
        record_only_tokens,
        retired_attempt_key,
        retired_started_entry_id,
        had_current_attempt,
    })
}
