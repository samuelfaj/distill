// Modified for Distill by Samuel Fajreldines, 2026.
//! Spawn admission: reparenting, duplicate checks, and the admit decision.

use tokio::sync::oneshot;

use super::super::admission::{AdmissionDecision, AdmissionError};
use super::super::coordinator_state::PendingChild;
use super::super::types::{SubagentOwner, SubagentRequest, SubagentResult, SubagentSpawnRequest};
use super::graph::NestedSpawner;
use super::queue::{QueuedCaller, QueuedSpawn, StartOrigin};
use super::{
    ChildRunOutput, ChildRunner, LimitedSpawnOrigin, SubagentCoordinator, SubagentLimitDecision,
    SubagentLimitNotice,
};

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn handle_spawn(&mut self, command: SubagentSpawnRequest) {
        let SubagentSpawnRequest {
            mut request,
            result_tx,
            mut registered_tx,
        } = command;
        // Registration is a side channel: a background caller still gets its terminal result on
        // `result_tx`. The scheduler actor needs the signal to tell "admitted" from a pre-start
        // reject before it deletes a one-shot.
        let start_ack = if request.run_in_background {
            BackgroundStartAck::OnRegister
        } else {
            BackgroundStartAck::Hold
        };
        if start_ack == BackgroundStartAck::Hold {
            registered_tx = None;
        }
        let spawner = match self.reparent_nested_spawn(&mut request) {
            Ok(spawner) => spawner,
            Err(rejection) => {
                let _ = result_tx.send(rejection);
                return;
            }
        };
        // Late Task spawn after user Stop (detached TaskTool background).
        if !request.owner.is_workflow()
            && self
                .spawn_blocked_sessions
                .contains(&request.parent_session_id)
        {
            let _ = result_tx.send(rejected_spawn_result(
                &request.id,
                "parent session is stopped",
                true,
            ));
            return;
        }
        let id = request.id.clone();
        if self.pending.contains_key(&id)
            || self.active.contains_key(&id)
            || self.completed.contains_key(&id)
            || self.queued.contains_id(&id)
        {
            let _ = result_tx.send(rejected_spawn_result(
                &id,
                &format!("Subagent id '{id}' already exists"),
                false,
            ));
            return;
        }
        // Capture before `insert_nested` moves `spawner`.
        let spawner_session_id = spawner.as_ref().map(|nested| nested.session_id.clone());
        // The node must exist before any record that can be looked up by `id`.
        match spawner {
            Some(spawner) => {
                // Unreachable while reparent only names spawners in `active`.
                if self
                    .graph
                    .insert_nested(&id, &request.parent_session_id, spawner)
                    .is_err()
                {
                    let _ = result_tx.send(rejected_spawn_result(
                        &id,
                        "parent subagent lineage is unknown; refusing to spawn",
                        true,
                    ));
                    return;
                }
            }
            None => self
                .graph
                .insert_root_child(&id, &request.parent_session_id),
        }
        let running = self.session_running_count(&request.parent_session_id);
        match self.admission.admit(&request, running) {
            AdmissionDecision::Start => self.start_child(
                *request,
                Some(result_tx),
                registered_tx,
                StartOrigin::Direct,
                None,
                spawner_session_id,
                None,
                None,
            ),
            AdmissionDecision::Enqueue => {
                debug_assert!(
                    !request.owner.is_workflow(),
                    "workflow spawns bypass admission and must never queue"
                );
                tracing::info!(
                    subagent_id = %request.id,
                    parent_session_id = %request.parent_session_id,
                    running,
                    "subagent queued at the concurrent limit"
                );
                self.notify_limit(
                    &request,
                    SubagentLimitDecision::QueuedAtConcurrentLimit {
                        limit: self.admission.max_concurrent(),
                    },
                );
                // Keep `result_tx` so cancel/completion of a queued background
                // child still resolves `spawn()`. Registration is a side channel.
                let deadline = request
                    .awaits_in_foreground()
                    .then(|| tokio::time::Instant::now() + self.config.foreground_budget);
                let agent_address = distill_message_delivery_core::mint_child_address(
                    request.owner.is_workflow(),
                    uuid::Uuid::new_v4().as_u128(),
                );
                self.queued.push_back(QueuedSpawn {
                    request,
                    queued_at: tokio::time::Instant::now(),
                    caller: QueuedCaller::Awaiting {
                        result_tx,
                        deadline,
                    },
                    agent_address,
                    spawner_session_id,
                    wake_origin: None,
                    wake: None,
                });
                // `Hold` already cleared `registered_tx`; fire iff the policy
                // left a signal.
                if let Some(tx) = registered_tx {
                    let _ = tx.send(());
                }
            }
            AdmissionDecision::Reject(error) => {
                self.notify_limit(
                    &request,
                    match &error {
                        AdmissionError::ConcurrentLimitReached { limit } => {
                            SubagentLimitDecision::RejectedAtConcurrentLimit { limit: *limit }
                        }
                    },
                );
                let result = SubagentResult::failed(id.clone(), id, error.message());
                self.finish_never_started(
                    *request,
                    Some(result_tx),
                    result,
                    std::time::Instant::now(),
                );
            }
        }
    }

    /// Re-key a nested spawn (its parent is itself a subagent) to the root
    /// session; the spawn graph keeps the ancestors' authority over it.
    fn reparent_nested_spawn(
        &self,
        request: &mut SubagentRequest,
    ) -> Result<Option<NestedSpawner>, SubagentResult> {
        let Some(spawner) = self.active_child_for_session(&request.parent_session_id) else {
            return Ok(None);
        };
        if spawner.cancellation.is_cancelled() {
            // The parent subagent is being torn down, so its late child
            // would be orphaned against the closed scope.
            return Err(rejected_spawn_result(
                &request.id,
                "parent subagent is being torn down",
                true,
            ));
        }
        let root_parent = spawner.request.parent_session_id.clone();
        let spawner_session_id = std::mem::replace(&mut request.parent_session_id, root_parent);
        // The request's flag becomes root-scoped; the spawner's wish lives on the graph node.
        let surface_completion = std::mem::replace(&mut request.surface_completion, false);
        // Nested children keep workflow lineage after reparent so
        // ParentSession Stop does not kill in-flight workflow work.
        if !request.owner.is_workflow()
            && let Some(run_id) = spawner.request.owner.workflow_run_id()
        {
            request.owner = SubagentOwner::workflow(run_id);
        }
        if request.runtime_overrides.loop_task_id.is_none() {
            request.runtime_overrides.loop_task_id =
                spawner.request.runtime_overrides.loop_task_id.clone();
        }
        Ok(Some(NestedSpawner {
            child_id: spawner.request.id.clone(),
            session_id: spawner_session_id,
            surface_completion,
        }))
    }

    /// Counts are computed here, not at call sites: a queued spawn counts
    /// itself in `queue_depth` (the notice fires before the push), a rejected
    /// spawn does not.
    pub(super) fn notify_limit(&self, request: &SubagentRequest, decision: SubagentLimitDecision) {
        let Some(sink) = &self.config.limit_sink else {
            return;
        };
        let queued = self.session_queued_count(&request.parent_session_id);
        sink(SubagentLimitNotice {
            parent_session_id: request.parent_session_id.clone(),
            decision,
            running: self.session_running_count(&request.parent_session_id),
            queue_depth: match decision {
                SubagentLimitDecision::QueuedAtConcurrentLimit { .. } => queued + 1,
                SubagentLimitDecision::RejectedAtConcurrentLimit { .. } => queued,
            },
            origin: if request.from_scheduler_loop() {
                LimitedSpawnOrigin::SchedulerLoop
            } else {
                LimitedSpawnOrigin::Task
            },
        });
    }

    /// Route a spawn that never reached the runner through `finish_child`,
    /// so waiters resolve and the id stays queryable; `since` anchors the
    /// record's duration.
    pub(super) fn finish_never_started(
        &mut self,
        mut request: SubagentRequest,
        spawn_reply: Option<oneshot::Sender<SubagentResult>>,
        result: SubagentResult,
        since: std::time::Instant,
    ) {
        let id = request.id.clone();
        drop(request.spawn_root.take_span());
        self.pending.insert(
            id.clone(),
            PendingChild {
                started_at: since,
                cancellation: request.cancel_token.clone(),
                spawn_reply,
                foreground_deadline: None,
                handle_only: request.run_in_background,
                explicitly_killed: false,
                disposition: Default::default(),
                launched: false,
                attempt_id: distill_message_delivery_core::AttemptId::mint(
                    uuid::Uuid::new_v4().as_u128(),
                ),
                generation: super::ActiveChildGeneration::new(),
                agent_address: None,
                spawner_session_id: None,
                wake_of: None,
                request,
            },
        );
        self.finish_child(
            &id,
            ChildRunOutput {
                result,
                completion_data: R::CompletionData::default(),
                snapshot_ref: None,
            },
        );
    }
}

/// Who should see a registration signal versus the terminal `result_tx` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackgroundStartAck {
    /// Fire `registered_tx` once the child is pending or queued.
    OnRegister,
    /// Leave `result_tx` for completion or a foreground-budget handoff.
    Hold,
}

/// A spawn refused before it ever became a child record.
fn rejected_spawn_result(id: &str, error: &str, cancelled: bool) -> SubagentResult {
    if cancelled {
        SubagentResult::cancelled(id, id, error)
    } else {
        SubagentResult::failed(id, id, error)
    }
}
