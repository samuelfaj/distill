// Modified for Distill by Samuel Fajreldines, 2026.
//! Restricted memory-v2 consolidation runner and model-plan decoder.

use super::*;

const V2_DREAM_MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const V2_DREAM_MODEL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const V2_DREAM_MIN_PENDING_COUNT: usize = 20;
const V2_DREAM_MAX_PENDING_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const V2_PROMOTION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const V2_PROMOTION_MAX_RETRIES: usize = 120;

/// Content-free `MemoryDreamCompleted.result` vocabulary; the disposition is the typed form.
pub(super) fn dream_notice(disposition: MemoryDreamDisposition) -> &'static str {
    match disposition {
        MemoryDreamDisposition::Busy => "busy",
        MemoryDreamDisposition::Failed => "failed",
        MemoryDreamDisposition::NoWork => "no work",
        MemoryDreamDisposition::Recovered => "recovered",
        MemoryDreamDisposition::RetryRequired => "retry required",
        MemoryDreamDisposition::Shadow => "shadow complete",
        MemoryDreamDisposition::Completed => "completed",
        MemoryDreamDisposition::Cancelled => "cancelled",
        MemoryDreamDisposition::Disabled => "disabled",
    }
}

/// One `execute_v2_dream` pass: its outcome, and whether a coalesced trigger wants another pass.
struct V2DreamPass {
    outcome: MemoryDreamResponse,
    coalesced: bool,
}

struct V2DreamModelFailure {
    class: distill_telemetry::memory_telemetry::MemoryV2FailureClass,
    detail: String,
    usage: distill_telemetry::memory_telemetry::MemoryV2ModelUsage,
}

fn classify_consolidation_error(
    error: &distill_memory::V2ConsolidationError,
) -> distill_telemetry::memory_telemetry::MemoryV2FailureClass {
    use distill_memory::V2ConsolidationError;
    use distill_telemetry::memory_telemetry::MemoryV2FailureClass;
    match error {
        V2ConsolidationError::Busy | V2ConsolidationError::StaleLease => {
            MemoryV2FailureClass::Lease
        }
        V2ConsolidationError::Access(_) => MemoryV2FailureClass::AccessPolicy,
        V2ConsolidationError::Invalid(_) => MemoryV2FailureClass::MalformedOutput,
        V2ConsolidationError::Conflict(_) | V2ConsolidationError::Convergence(_) => {
            MemoryV2FailureClass::Convergence
        }
        V2ConsolidationError::Database(_) | V2ConsolidationError::Io { .. } => {
            MemoryV2FailureClass::Storage
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct V2DreamPlan {
    operations: Vec<V2DreamPlanOperation>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum V2DreamPlanOperation {
    Create {
        path: String,
        content: String,
        evidence: Vec<String>,
    },
    Update {
        path: String,
        content: String,
        evidence: Vec<String>,
    },
    Delete {
        path: String,
        evidence: Vec<String>,
    },
    Rename {
        from: String,
        to: String,
        content: String,
        evidence: Vec<String>,
    },
    Merge {
        sources: Vec<String>,
        destination: String,
        content: String,
        evidence: Vec<String>,
    },
    Split {
        source: String,
        destinations: Vec<V2DreamDestination>,
        evidence: Vec<String>,
    },
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct V2DreamDestination {
    path: String,
    content: String,
}

/// Bare JSON schema for `ConversationRequest::json_schema`, mirroring `V2DreamPlan`. The sampling
/// client adds the `{name, strict, schema}` envelope.
fn dream_plan_schema() -> serde_json::Value {
    let strings = serde_json::json!({ "type": "array", "items": { "type": "string" } });
    let operation = |op: &str, fields: &[(&str, serde_json::Value)]| {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "op".to_owned(),
            serde_json::json!({ "type": "string", "enum": [op] }),
        );
        let mut required = vec![serde_json::Value::from("op")];
        for (name, schema) in fields {
            properties.insert((*name).to_owned(), schema.clone());
            required.push(serde_json::Value::from(*name));
        }
        properties.insert("evidence".to_owned(), strings.clone());
        required.push(serde_json::Value::from("evidence"));
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": required,
            "properties": properties
        })
    };
    let string = serde_json::json!({ "type": "string" });
    let destination = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "content"],
        "properties": { "path": string, "content": string }
    });
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["operations"],
        "properties": {
            "operations": {
                "type": "array",
                "items": {
                    "anyOf": [
                        operation("create", &[("path", string.clone()), ("content", string.clone())]),
                        operation("update", &[("path", string.clone()), ("content", string.clone())]),
                        operation("delete", &[("path", string.clone())]),
                        operation("rename", &[("from", string.clone()), ("to", string.clone()), ("content", string.clone())]),
                        operation("merge", &[("sources", strings.clone()), ("destination", string.clone()), ("content", string.clone())]),
                        operation("split", &[("source", string.clone()), ("destinations", serde_json::json!({ "type": "array", "items": destination }))]),
                    ]
                }
            }
        }
    })
}

enum V2DreamClaimOutcome {
    NoWork,
    Resumed(distill_memory::ConsolidationResult),
    Fresh {
        guard: V2DreamLeaseGuard,
        input: distill_memory::ConsolidationInput,
    },
}

struct V2DreamLeaseGuard {
    resources: Option<(
        Box<distill_memory::V2ConsolidationStore>,
        distill_memory::ConsolidationLease,
    )>,
    clock: distill_memory::SharedV2Clock,
}

struct V2DreamCancellationGuard(tokio_util::sync::CancellationToken);

impl Drop for V2DreamCancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl V2DreamLeaseGuard {
    fn new(
        store: Box<distill_memory::V2ConsolidationStore>,
        lease: distill_memory::ConsolidationLease,
        clock: distill_memory::SharedV2Clock,
    ) -> Self {
        Self {
            resources: Some((store, lease)),
            clock,
        }
    }

    fn store(&self) -> &distill_memory::V2ConsolidationStore {
        let Some((store, _)) = self.resources.as_ref() else {
            unreachable!("an armed Dream lease guard always owns its store");
        };
        store
    }

    fn lease(&self) -> &distill_memory::ConsolidationLease {
        let Some((_, lease)) = self.resources.as_ref() else {
            unreachable!("an armed Dream lease guard always owns its lease");
        };
        lease
    }

    fn into_parts(
        mut self,
    ) -> (
        Box<distill_memory::V2ConsolidationStore>,
        distill_memory::ConsolidationLease,
    ) {
        let Some(resources) = self.resources.take() else {
            unreachable!("an armed Dream lease guard always owns its resources");
        };
        resources
    }

    fn fail_now(mut self, now: i64, error: &str) {
        if let Some((store, lease)) = self.resources.take()
            && let Err(release_error) = store.fail_retryable(&lease, now, error)
        {
            tracing::warn!(error = %release_error, "memory-v2 Dream lease release failed");
        }
    }

    async fn fail_retryable(mut self, error: impl Into<String>) {
        let Some((store, lease)) = self.resources.take() else {
            return;
        };
        let error = error.into();
        let clock = self.clock.clone();
        match tokio::task::spawn_blocking(move || {
            store.fail_retryable(&lease, clock.now_unix_seconds(), &error)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(release_error)) => {
                tracing::warn!(error = %release_error, "memory-v2 Dream lease release failed");
            }
            Err(join_error) => {
                tracing::warn!(error = %join_error, "memory-v2 Dream lease release task failed");
            }
        }
    }
}

impl Drop for V2DreamLeaseGuard {
    fn drop(&mut self) {
        let Some((store, lease)) = self.resources.take() else {
            return;
        };
        let clock = self.clock.clone();
        let release = move || {
            if let Err(error) =
                store.fail_retryable(&lease, clock.now_unix_seconds(), "Dream task cancelled")
            {
                tracing::warn!(error = %error, "cancelled memory-v2 Dream lease release failed");
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(release);
        } else {
            release();
        }
    }
}

#[derive(Clone, Copy)]
enum V2DreamInvocation {
    Automatic,
    Manual,
}

fn v2_dream_invocation_enabled(
    config: crate::config::MemoryV2Config,
    invocation: V2DreamInvocation,
) -> bool {
    match invocation {
        V2DreamInvocation::Automatic => config.can_run_automatic_dream(),
        V2DreamInvocation::Manual => config.can_run_manual_dream(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct V2CaptureFollowups {
    run_maintenance: bool,
    evaluate_automatic_dream: bool,
}

fn v2_capture_followups(config: crate::config::MemoryV2Config) -> V2CaptureFollowups {
    V2CaptureFollowups {
        run_maintenance: config.can_run_maintenance(),
        evaluate_automatic_dream: config.can_run_automatic_dream(),
    }
}

fn emit_v2_dream_lifecycle(
    disposition: distill_telemetry::memory_telemetry::MemoryV2DreamDisposition,
    observation_count: usize,
    topic_change_count: usize,
    started_at: std::time::Instant,
    failure_class: Option<distill_telemetry::memory_telemetry::MemoryV2FailureClass>,
) {
    distill_telemetry::session_ctx::log_event(
        distill_telemetry::memory_telemetry::MemoryV2DreamLifecycle {
            disposition,
            observation_count,
            topic_change_count,
            latency_ms: started_at.elapsed().as_millis() as u64,
            failure_class,
            usage: distill_telemetry::memory_telemetry::MemoryV2ModelUsage::default(),
        },
    );
}

fn emit_v2_dream_lifecycle_with_usage(
    session: &SessionActor,
    disposition: distill_telemetry::memory_telemetry::MemoryV2DreamDisposition,
    observation_count: usize,
    topic_change_count: usize,
    started_at: std::time::Instant,
    failure_class: Option<distill_telemetry::memory_telemetry::MemoryV2FailureClass>,
    usage: distill_telemetry::memory_telemetry::MemoryV2ModelUsage,
) {
    session.memory.record_dream_usage(&usage);
    distill_telemetry::session_ctx::log_event(
        distill_telemetry::memory_telemetry::MemoryV2DreamLifecycle {
            disposition,
            observation_count,
            topic_change_count,
            latency_ms: started_at.elapsed().as_millis() as u64,
            failure_class,
            usage,
        },
    );
}

fn parse_v2_dream_plan(response: &str) -> Result<Vec<distill_memory::TopicOperation>, String> {
    let response = response.trim();
    let response = response
        .strip_prefix("```json")
        .or_else(|| response.strip_prefix("```"))
        .unwrap_or(response);
    let response = response.strip_suffix("```").unwrap_or(response).trim();
    let plan: V2DreamPlan =
        serde_json::from_str(response).map_err(|error| format!("invalid Dream plan: {error}"))?;
    if plan.operations.is_empty() {
        return Err("invalid Dream plan: operations must not be empty".to_owned());
    }
    let paths = |values: Vec<String>| values.into_iter().map(std::path::PathBuf::from).collect();
    Ok(plan
        .operations
        .into_iter()
        .map(|operation| match operation {
            V2DreamPlanOperation::Create {
                path,
                content,
                evidence,
            } => distill_memory::TopicOperation::Create {
                path: path.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Update {
                path,
                content,
                evidence,
            } => distill_memory::TopicOperation::Update {
                path: path.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Delete { path, evidence } => {
                distill_memory::TopicOperation::Delete {
                    path: path.into(),
                    evidence: paths(evidence),
                }
            }
            V2DreamPlanOperation::Rename {
                from,
                to,
                content,
                evidence,
            } => distill_memory::TopicOperation::Rename {
                from: from.into(),
                to: to.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Merge {
                sources,
                destination,
                content,
                evidence,
            } => distill_memory::TopicOperation::Merge {
                sources: paths(sources),
                destination: destination.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Split {
                source,
                destinations,
                evidence,
            } => distill_memory::TopicOperation::Split {
                source: source.into(),
                destinations: destinations
                    .into_iter()
                    .map(|destination| (destination.path.into(), destination.content))
                    .collect(),
                evidence: paths(evidence),
            },
        })
        .collect())
}

impl SessionActor {
    pub(super) async fn promote_v2_hidden_observations(
        &self,
        retry_deferred: bool,
        clock: distill_memory::SharedV2Clock,
    ) {
        if !self.memory.can_expose_v2() {
            return;
        }
        let Some(storage) = self.memory.storage() else {
            return;
        };
        let workspace = storage.workspace_dir().to_path_buf();
        let cancel = self.memory.dream_workers.cancellation_token();
        for attempt in 0..=V2_PROMOTION_MAX_RETRIES {
            if cancel.is_cancelled() {
                return;
            }
            let attempt_workspace = workspace.clone();
            let attempt_clock = clock.clone();
            match tokio::task::spawn_blocking(move || {
                distill_memory::V2CaptureStore::open_with_clock(
                    &attempt_workspace,
                    distill_memory::V2MemoryScope::Workspace,
                    attempt_clock,
                )?
                .promote_hidden_observations_now()
            })
            .await
            {
                Ok(Ok(_)) => return,
                Ok(Err(error))
                    if retry_deferred
                        && attempt < V2_PROMOTION_MAX_RETRIES
                        && matches!(
                            error,
                            distill_memory::V2CaptureError::Conflict(_)
                                | distill_memory::V2CaptureError::Database(_)
                                | distill_memory::V2CaptureError::Io { .. }
                        ) =>
                {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(V2_PROMOTION_RETRY_INTERVAL) => {}
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "memory-v2 active promotion deferred");
                    return;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "memory-v2 active promotion task panicked");
                    return;
                }
            }
        }
    }

    pub(super) async fn on_v2_capture_completed(
        self: &Arc<Self>,
        cancel: tokio_util::sync::CancellationToken,
        clock: distill_memory::SharedV2Clock,
    ) {
        use distill_telemetry::memory_telemetry::{
            MemoryV2Component, MemoryV2FailClosed, MemoryV2FailureClass,
        };
        let followups = v2_capture_followups(self.memory.v2_config);
        let Some(storage) = self.memory.storage() else {
            return;
        };
        let global = storage.global_dir().to_path_buf();
        let workspace = storage.workspace_dir().to_path_buf();
        if followups.run_maintenance {
            let retention = distill_memory::RetentionPolicy {
                archived_observation_days: self.memory.v2_config.archived_retention_days,
                terminal_job_days: self.memory.v2_config.job_retention_days,
            };
            let maintenance_global = global.clone();
            let maintenance_workspace = workspace.clone();
            let maintenance_clock = clock.clone();
            match tokio::task::spawn_blocking(move || {
                let maintenance = distill_memory::V2MaintenanceStore::open_with_clock(
                    &maintenance_workspace,
                    distill_memory::V2MemoryScope::Workspace,
                    &maintenance_global,
                    &maintenance_workspace,
                    maintenance_clock,
                )?;
                maintenance.gc_now(retention)
            })
            .await
            {
                Ok(Ok(result)) => {
                    distill_telemetry::session_ctx::log_event(
                        distill_telemetry::memory_telemetry::MemoryV2GcCompleted {
                            archived_observations_removed: result.archived_observations_removed,
                            terminal_jobs_removed: result.terminal_jobs_removed,
                        },
                    );
                }
                Ok(Err(error)) => {
                    let reason = match &error {
                        distill_memory::V2MaintenanceError::ActiveLease => {
                            MemoryV2FailureClass::Lease
                        }
                        distill_memory::V2MaintenanceError::Access(_) => {
                            MemoryV2FailureClass::AccessPolicy
                        }
                        distill_memory::V2MaintenanceError::Index(_)
                        | distill_memory::V2MaintenanceError::Manifest(_) => {
                            MemoryV2FailureClass::Convergence
                        }
                        _ => MemoryV2FailureClass::Storage,
                    };
                    tracing::warn!(error = %error, "memory-v2 maintenance failed");
                    distill_telemetry::session_ctx::log_event(MemoryV2FailClosed {
                        component: MemoryV2Component::GarbageCollection,
                        reason,
                    });
                }
                Err(error) => {
                    tracing::warn!(error = %error, "memory-v2 maintenance task panicked");
                    distill_telemetry::session_ctx::log_event(MemoryV2FailClosed {
                        component: MemoryV2Component::GarbageCollection,
                        reason: MemoryV2FailureClass::Convergence,
                    });
                }
            }
        }
        if !followups.evaluate_automatic_dream {
            return;
        }
        let is_shadow = self.memory.v2_config.rollout == crate::config::MemoryV2Rollout::Shadow;
        let started_at = std::time::Instant::now();
        let eligibility_clock = clock.clone();
        let eligibility = tokio::task::spawn_blocking(move || {
            let store = distill_memory::V2ConsolidationStore::open_with_clock(
                &workspace,
                distill_memory::V2MemoryScope::Workspace,
                &global,
                &workspace,
                eligibility_clock.clone(),
            )?;
            let now = eligibility_clock.now_unix_seconds();
            let config = distill_memory::DreamEligibilityConfig {
                min_pending_count: V2_DREAM_MIN_PENDING_COUNT,
                max_pending_age: V2_DREAM_MAX_PENDING_AGE,
            };
            let eligibility = if is_shadow {
                store.on_capture_completed_shadow(now, config)
            } else {
                store.on_capture_completed(now, config)
            }?;
            Ok::<_, distill_memory::V2ConsolidationError>(eligibility)
        })
        .await;
        if cancel.is_cancelled() {
            return;
        }
        match eligibility {
            Ok(Ok(eligibility))
                if eligibility.disposition == distill_memory::DreamTriggerDisposition::Ready =>
            {
                emit_v2_dream_lifecycle(
                    distill_telemetry::memory_telemetry::MemoryV2DreamDisposition::Ready,
                    eligibility.pending_count,
                    0,
                    started_at,
                    None,
                );
                self.run_v2_dream_with_cancel(V2DreamInvocation::Automatic, cancel, clock)
                    .await;
            }
            Ok(Ok(eligibility)) => {
                let disposition = match eligibility.disposition {
                    distill_memory::DreamTriggerDisposition::Ineligible => {
                        distill_telemetry::memory_telemetry::MemoryV2DreamDisposition::Ineligible
                    }
                    distill_memory::DreamTriggerDisposition::Coalesced => {
                        distill_telemetry::memory_telemetry::MemoryV2DreamDisposition::Coalesced
                    }
                    distill_memory::DreamTriggerDisposition::Ready => {
                        distill_telemetry::memory_telemetry::MemoryV2DreamDisposition::Ready
                    }
                };
                emit_v2_dream_lifecycle(
                    disposition,
                    eligibility.pending_count,
                    0,
                    started_at,
                    None,
                );
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "memory-v2 Dream eligibility update failed");
            }
            Err(error) => {
                tracing::warn!(error = %error, "memory-v2 Dream eligibility task panicked");
            }
        }
    }

    pub(super) async fn run_v2_dream_slash_command(self: &Arc<Self>) -> MemoryDreamResponse {
        self.run_v2_dream_with_cancel(
            V2DreamInvocation::Manual,
            self.memory.dream_workers.cancellation_token(),
            distill_memory::system_v2_clock(),
        )
        .await
    }

    /// Runs Dream passes until no coalesced trigger remains; the last pass's outcome is returned.
    async fn run_v2_dream_with_cancel(
        self: &Arc<Self>,
        invocation: V2DreamInvocation,
        cancel: tokio_util::sync::CancellationToken,
        clock: distill_memory::SharedV2Clock,
    ) -> MemoryDreamResponse {
        use distill_telemetry::memory_telemetry::{
            MemoryV2Component, MemoryV2FailClosed, MemoryV2FailureClass,
        };
        if !v2_dream_invocation_enabled(self.memory.v2_config, invocation) {
            distill_telemetry::session_ctx::log_event(MemoryV2FailClosed {
                component: MemoryV2Component::Dream,
                reason: MemoryV2FailureClass::Disabled,
            });
            return MemoryDreamResponse::new(MemoryDreamDisposition::Disabled);
        }
        let cancel = cancel.child_token();
        if cancel.is_cancelled() {
            return MemoryDreamResponse::new(MemoryDreamDisposition::Cancelled);
        }
        let cancel_on_drop = V2DreamCancellationGuard(cancel.clone());
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        let session = Arc::clone(self);
        let task = distill_telemetry::session_ctx::spawn_local_in_session_ctx(async move {
            let mut outcome = MemoryDreamResponse::new(MemoryDreamDisposition::Cancelled);
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let pass = session.execute_v2_dream(&cancel, clock.clone()).await;
                outcome = outcome.then(pass.outcome);
                // Promotion can be deferred by the lease this Dream just released.
                // Retrying here makes rollout activation converge without requiring
                // a session restart or another completed turn.
                session
                    .promote_v2_hidden_observations(false, clock.clone())
                    .await;
                if !pass.coalesced {
                    break;
                }
            }
            let _ = completed_tx.send(outcome);
        });
        self.memory.dream_workers.track(task);
        let outcome = completed_rx
            .await
            .unwrap_or(MemoryDreamResponse::new(MemoryDreamDisposition::Failed));
        drop(cancel_on_drop);
        outcome
    }

    /// Sends the completion notification and packages the pass result.
    async fn finish_v2_dream(
        &self,
        disposition: MemoryDreamDisposition,
        observation_count: usize,
        topics_affected: usize,
        coalesced: bool,
    ) -> V2DreamPass {
        self.send_xai_notification(XaiSessionUpdate::MemoryDreamCompleted {
            result: dream_notice(disposition).to_owned(),
            path: None,
        })
        .await;
        V2DreamPass {
            outcome: MemoryDreamResponse {
                disposition,
                observation_count,
                topics_affected,
            },
            coalesced,
        }
    }

    /// Execute one claim. `coalesced` is set when a trigger that arrived during
    /// the pass requires another iteration in the same tracked caller task.
    async fn execute_v2_dream(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
        clock: distill_memory::SharedV2Clock,
    ) -> V2DreamPass {
        use distill_telemetry::memory_telemetry::{MemoryV2DreamDisposition, MemoryV2FailureClass};
        let cancelled = |coalesced| V2DreamPass {
            outcome: MemoryDreamResponse::new(MemoryDreamDisposition::Cancelled),
            coalesced,
        };
        let started_at = std::time::Instant::now();
        if cancel.is_cancelled() {
            return cancelled(false);
        }
        let Some(storage) = self.memory.storage() else {
            return V2DreamPass {
                outcome: MemoryDreamResponse::new(MemoryDreamDisposition::Disabled),
                coalesced: false,
            };
        };
        self.send_xai_notification(XaiSessionUpdate::MemoryDreamQueued)
            .await;
        let owner = format!("session-{}", self.session_info.id);
        let now = clock.now_unix_seconds();
        let global = storage.global_dir().to_path_buf();
        let workspace = storage.workspace_dir().to_path_buf();
        let is_shadow = self.memory.v2_config.rollout == crate::config::MemoryV2Rollout::Shadow;
        let claimed = tokio::task::spawn_blocking({
            let global = global.clone();
            let workspace = workspace.clone();
            let store_clock = clock.clone();
            move || {
                let store = distill_memory::V2ConsolidationStore::open_with_clock(
                    &workspace,
                    distill_memory::V2MemoryScope::Workspace,
                    &global,
                    &workspace,
                    store_clock.clone(),
                )?;
                let request = distill_memory::DreamClaimRequest {
                    owner,
                    now,
                    duration: V2_DREAM_MODEL_TIMEOUT.saturating_mul(2),
                };
                let lease = if is_shadow {
                    store.claim_shadow(&request)?
                } else {
                    store.claim(&request)?
                };
                let Some(lease) = lease else {
                    return Ok::<_, distill_memory::V2ConsolidationError>(
                        V2DreamClaimOutcome::NoWork,
                    );
                };
                let guard = V2DreamLeaseGuard::new(Box::new(store), lease, store_clock);
                let resumed = guard.store().resume_planned(guard.lease(), now);
                let resumed = match resumed {
                    Ok(resumed) => resumed,
                    Err(error) => {
                        guard.fail_now(now, "Dream plan recovery failed");
                        return Err(error);
                    }
                };
                if let Some(result) = resumed {
                    return Ok(V2DreamClaimOutcome::Resumed(result));
                }
                let input = match guard.store().consolidation_input(guard.lease(), now) {
                    Ok(input) => input,
                    Err(error) => {
                        guard.fail_now(now, "Dream input preparation failed");
                        return Err(error);
                    }
                };
                Ok(V2DreamClaimOutcome::Fresh { guard, input })
            }
        })
        .await;
        let outcome = match claimed {
            Ok(Ok(claimed)) => claimed,
            Ok(Err(distill_memory::V2ConsolidationError::Busy)) => {
                emit_v2_dream_lifecycle(
                    MemoryV2DreamDisposition::Busy,
                    0,
                    0,
                    started_at,
                    Some(MemoryV2FailureClass::Lease),
                );
                return self
                    .finish_v2_dream(MemoryDreamDisposition::Busy, 0, 0, false)
                    .await;
            }
            Ok(Err(error)) => {
                emit_v2_dream_lifecycle(
                    MemoryV2DreamDisposition::Failed,
                    0,
                    0,
                    started_at,
                    Some(classify_consolidation_error(&error)),
                );
                tracing::warn!(error = %error, "memory-v2 Dream claim failed");
                return self
                    .finish_v2_dream(MemoryDreamDisposition::Failed, 0, 0, false)
                    .await;
            }
            Err(error) => {
                emit_v2_dream_lifecycle(
                    MemoryV2DreamDisposition::Failed,
                    0,
                    0,
                    started_at,
                    Some(MemoryV2FailureClass::Convergence),
                );
                tracing::warn!(error = %error, "memory-v2 Dream claim task panicked");
                return self
                    .finish_v2_dream(MemoryDreamDisposition::Failed, 0, 0, false)
                    .await;
            }
        };
        let (guard, input) = match outcome {
            V2DreamClaimOutcome::NoWork => {
                emit_v2_dream_lifecycle(MemoryV2DreamDisposition::Noop, 0, 0, started_at, None);
                self.memory.record_dream_neutral();
                return self
                    .finish_v2_dream(MemoryDreamDisposition::NoWork, 0, 0, false)
                    .await;
            }
            V2DreamClaimOutcome::Resumed(result) => {
                emit_v2_dream_lifecycle(
                    MemoryV2DreamDisposition::Reconciled,
                    0,
                    result.affected_topics.len(),
                    started_at,
                    None,
                );
                self.memory.record_dream_result(true);
                return self
                    .finish_v2_dream(
                        MemoryDreamDisposition::Recovered,
                        0,
                        result.affected_topics.len(),
                        false,
                    )
                    .await;
            }
            V2DreamClaimOutcome::Fresh { guard, input } => (guard, input),
        };
        if cancel.is_cancelled() {
            guard.fail_retryable("Dream task cancelled").await;
            return cancelled(true);
        }
        self.send_xai_notification(XaiSessionUpdate::MemoryDreamStarted {
            observation_count: input.observations.len(),
        })
        .await;

        let prompt = serde_json::json!({
            "operation_id": input.operation_id,
            "claimed_observations": input.observations,
            "existing_topics": input.topics,
        })
        .to_string();
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                guard.fail_retryable("Dream task cancelled").await;
                return cancelled(true);
            },
            response = self.run_v2_dream_model_call(&prompt) => response,
        };
        let (operations, usage) = match response.and_then(|(text, usage)| {
            parse_v2_dream_plan(&text)
                .map(|operations| (operations, usage.clone()))
                .map_err(|detail| V2DreamModelFailure {
                    class: MemoryV2FailureClass::MalformedOutput,
                    detail,
                    usage,
                })
        }) {
            Ok(result) => result,
            Err(error) => {
                let observation_count = guard.lease().observations.len();
                emit_v2_dream_lifecycle_with_usage(
                    self,
                    MemoryV2DreamDisposition::Retry,
                    observation_count,
                    0,
                    started_at,
                    Some(error.class),
                    error.usage,
                );
                let detail = error.detail;
                guard.fail_retryable(detail.clone()).await;
                self.memory.record_dream_result(false);
                tracing::warn!(error = %detail, "memory-v2 Dream model or plan failed");
                return self
                    .finish_v2_dream(
                        MemoryDreamDisposition::RetryRequired,
                        observation_count,
                        0,
                        false,
                    )
                    .await;
            }
        };
        if cancel.is_cancelled() {
            guard.fail_retryable("Dream task cancelled").await;
            return cancelled(true);
        }
        let (store, lease) = guard.into_parts();
        let observation_count = lease.observations.len();
        if self.memory.v2_config.rollout == crate::config::MemoryV2Rollout::Shadow {
            let shadow_clock = clock.clone();
            let shadow_result = tokio::task::spawn_blocking(move || {
                let now = shadow_clock.now_unix_seconds();
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    store.complete_shadow(&lease, &operations, now)
                })) {
                    Ok(result) => result,
                    Err(payload) => {
                        // A blocking-task panic must not strand the lease until
                        // expiry. Release it before preserving panic semantics.
                        let _ = store.fail_retryable(&lease, now, "shadow completion panicked");
                        std::panic::resume_unwind(payload);
                    }
                };
                let coalesced = result
                    .as_ref()
                    .is_ok_and(|_| store.has_coalesced_trigger().unwrap_or(false));
                (result, coalesced)
            })
            .await;
            return match shadow_result {
                Ok((Ok(_), coalesced)) => {
                    self.memory.record_dream_result(true);
                    emit_v2_dream_lifecycle_with_usage(
                        self,
                        MemoryV2DreamDisposition::Shadow,
                        observation_count,
                        0,
                        started_at,
                        None,
                        usage.clone(),
                    );
                    self.finish_v2_dream(
                        MemoryDreamDisposition::Shadow,
                        observation_count,
                        0,
                        coalesced,
                    )
                    .await
                }
                Ok((Err(error), _)) => {
                    self.memory.record_dream_result(false);
                    emit_v2_dream_lifecycle_with_usage(
                        self,
                        MemoryV2DreamDisposition::Retry,
                        observation_count,
                        0,
                        started_at,
                        Some(classify_consolidation_error(&error)),
                        usage.clone(),
                    );
                    tracing::warn!(error = %error, "memory-v2 shadow completion failed");
                    self.finish_v2_dream(
                        MemoryDreamDisposition::RetryRequired,
                        observation_count,
                        0,
                        false,
                    )
                    .await
                }
                Err(error) => {
                    self.memory.record_dream_result(false);
                    emit_v2_dream_lifecycle_with_usage(
                        self,
                        MemoryV2DreamDisposition::Failed,
                        observation_count,
                        0,
                        started_at,
                        Some(MemoryV2FailureClass::Convergence),
                        usage.clone(),
                    );
                    tracing::warn!(error = %error, "memory-v2 shadow completion task panicked");
                    self.finish_v2_dream(
                        MemoryDreamDisposition::Failed,
                        observation_count,
                        0,
                        false,
                    )
                    .await
                }
            };
        }
        let commit_clock = clock;
        let committed = tokio::task::spawn_blocking(move || {
            let now = commit_clock.now_unix_seconds();
            let result = store.commit(&lease, &operations, now);
            if let Err(error) = &result {
                let _ = store.fail_retryable(&lease, now, &format!("Dream commit failed: {error}"));
            }
            let coalesced = result
                .as_ref()
                .is_ok_and(|_| store.has_coalesced_trigger().unwrap_or(false));
            (result, coalesced)
        })
        .await;
        match committed {
            Ok((Ok(result), coalesced)) => {
                emit_v2_dream_lifecycle_with_usage(
                    self,
                    MemoryV2DreamDisposition::Committed,
                    observation_count,
                    result.affected_topics.len(),
                    started_at,
                    None,
                    usage.clone(),
                );
                self.memory.record_dream_result(true);
                self.finish_v2_dream(
                    MemoryDreamDisposition::Completed,
                    observation_count,
                    result.affected_topics.len(),
                    coalesced,
                )
                .await
            }
            Ok((Err(error), _)) => {
                emit_v2_dream_lifecycle_with_usage(
                    self,
                    MemoryV2DreamDisposition::Retry,
                    observation_count,
                    0,
                    started_at,
                    Some(classify_consolidation_error(&error)),
                    usage.clone(),
                );
                self.memory.record_dream_result(false);
                tracing::warn!(error = %error, "memory-v2 Dream commit failed");
                self.finish_v2_dream(
                    MemoryDreamDisposition::RetryRequired,
                    observation_count,
                    0,
                    false,
                )
                .await
            }
            Err(error) => {
                emit_v2_dream_lifecycle_with_usage(
                    self,
                    MemoryV2DreamDisposition::Failed,
                    observation_count,
                    0,
                    started_at,
                    Some(MemoryV2FailureClass::Convergence),
                    usage,
                );
                self.memory.record_dream_result(false);
                tracing::warn!(error = %error, "memory-v2 Dream commit task panicked");
                self.finish_v2_dream(MemoryDreamDisposition::Failed, observation_count, 0, false)
                    .await
            }
        }
    }

    async fn run_v2_dream_model_call(
        &self,
        input: &str,
    ) -> Result<
        (
            String,
            distill_telemetry::memory_telemetry::MemoryV2ModelUsage,
        ),
        V2DreamModelFailure,
    > {
        use distill_telemetry::memory_telemetry::MemoryV2FailureClass;
        const V2_DREAM_SYSTEM_PROMPT: &str = "Consolidate only the supplied claimed observations \
            into the supplied curated topics. Topics are reference notes for a future agent that has \
            not seen any conversation and will read them before starting related work. A topic covers \
            one broad subject area (a system, a repository area, a tool, a person, or a workflow); its \
            sub-areas are `##` sections, not separate topics. A workspace usually needs fewer than ten \
            topics. Treat `topic_hint` as a section suggestion; add to the existing topic whose area \
            covers an observation rather than creating a new one, and split only above about 16 KB. \
            Each topic starts with a `# Title` and one sentence stating what it covers (an index shows \
            only those two lines), then durable facts as short standalone statements under `##` \
            headings: file paths, constants, commands, invariants, decisions, and user preferences. \
            Put changing state under `## Recent state` or omit it. Do not narrate conversations. Drop \
            facts that newer observations contradict or make stale. \
            Return one JSON object with an `operations` array. \
            Every operation must have an `op` field identifying its type: create, update, delete, \
            rename, merge, or split. Use exactly the fields listed for each type; do not add other fields. \
            create and update: op, path, content, evidence. delete: op, path, evidence. \
            rename: op, from, to, content, evidence. merge: op, sources, destination, content, evidence. \
            split: op, source, destinations, evidence. evidence and sources are arrays of path strings. \
            destinations is an array of objects containing exactly path and content. All other fields \
            are strings. Example: {\"operations\":[{\"op\":\"create\",\"path\":\"topics/preferences.md\",\
            \"content\":\"# Reply preferences\\nHow the user wants answers written.\\n\\n## Style\\n\
            - Prefer concise answers.\",\"evidence\":[\"observations/_inbox/example.md\"]}]}. \
            The example is illustrative; use only facts and exact evidence paths supplied in the input. \
            Every operation must cite one or more exact claimed observation paths in `evidence`. Every topic path \
            must be exactly `topics/<slug>.md`: one markdown file directly in `topics/`, no subdirectories. A slug is \
            lowercase letters, digits, and hyphens, for example `topics/preferences.md`. Preserve still-valid facts. \
            Do not include `<!-- memory-v2 provenance -->` comments in content; they are stripped and not stored. \
            Do not mention or attempt tools, shell, workspace files, MEMORY.md, databases, archive, \
            extraction, or another Dream.";
        let sampling_client =
            self.prepare_chat_completion(false)
                .await
                .map_err(|error| V2DreamModelFailure {
                    class: MemoryV2FailureClass::Model,
                    detail: error.to_string(),
                    usage: distill_telemetry::memory_telemetry::MemoryV2ModelUsage::default(),
                })?;
        let model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|config| config.model)
            .unwrap_or_default();
        let (model, reasoning_effort) =
            super::memory_capture::resolve_memory_model_and_effort(&self.models_manager, model);
        let request = ConversationRequest {
            items: vec![
                ConversationItem::system(V2_DREAM_SYSTEM_PROMPT),
                ConversationItem::user(input),
            ],
            tools: vec![],
            model: Some(model.clone()),
            reasoning_effort,
            json_schema: Some(dream_plan_schema()),
            x_grok_conv_id: Some(format!("dream-v2-{}", uuid::Uuid::new_v4())),
            x_grok_req_id: Some(format!("xai-dream-v2-{}", uuid::Uuid::new_v4())),
            x_grok_session_id: Some(self.session_info.id.to_string()),
            x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
            ..Default::default()
        };
        tokio::time::timeout(
            V2_DREAM_MODEL_TIMEOUT,
            sampling_client
                .conversation_collect_with_idle_timeout(request, V2_DREAM_MODEL_IDLE_TIMEOUT),
        )
        .await
        .map_err(|_| V2DreamModelFailure {
            class: MemoryV2FailureClass::Timeout,
            detail: "v2 Dream model call timed out".to_owned(),
            usage: distill_telemetry::memory_telemetry::MemoryV2ModelUsage::default(),
        })?
        .map(|response| {
            let usage =
                crate::session::memory_observation::memory_v2_model_usage(&model, &response);
            (response.assistant_text(), usage)
        })
        .map_err(|error| V2DreamModelFailure {
            class: MemoryV2FailureClass::Model,
            detail: format!("v2 Dream model call failed: {error}"),
            usage: distill_telemetry::memory_telemetry::MemoryV2ModelUsage::default(),
        })
    }
}

#[cfg(test)]
#[path = "v2_memory_dream_tests.rs"]
mod tests;
