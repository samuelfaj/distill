// Modified for Distill by Samuel Fajreldines, 2026.
//! Goal handling for `SessionActor`.

use super::*;

/// Minimum toolset a role needs, checked by the parent-side gate.
/// A configured harness `agent_type` whose role toolset lacks the capability fails open to the current model and session harness.
/// Failing open beats spawning an unusable verifier.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RoleCapability {
    /// Reads and greps code to corroborate diff hunks.
    Skeptic,
    /// Reads, greps, and runs commands while investigating traces.
    Strategist,
}

impl RoleCapability {
    /// `true` when `summary`'s toolset satisfies this role's minimum capability.
    /// Keyed on the `can_*` flags (`can_search` for grep, `can_execute` for terminal/bash).
    fn is_satisfied(
        self,
        summary: &distill_tools::implementations::distill::task::types::SubagentTypeSummary,
    ) -> bool {
        match self {
            Self::Skeptic => summary.can_read && summary.can_search,
            Self::Strategist => summary.can_read && summary.can_search && summary.can_execute,
        }
    }
}

/// Panel-scoped memoization for `resolve_goal_role_override`: at most one `describe_subagent_type` coordinator round-trip per distinct `agent_type`.
/// So a multi-index skeptic panel sharing one agent_type costs one round-trip instead of N.
/// A single planner/strategist resolve uses a fresh (empty) cache; no cross-call sharing is needed.
#[derive(Default)]
pub(crate) struct PanelResolveCache {
    /// Maps a harness `agent_type` to its describe outcome (the coordinator's answer for the role's `general-purpose` toolset on that harness).
    describe: std::collections::HashMap<
        String,
        distill_tools::implementations::distill::task::types::SubagentDescribeOutcome,
    >,
}

/// Build a role's prompt tool names from its resolved spawn override.
/// A committed explicit pair draws its names from the same `describe_subagent_type` summary cached during the gate, with no second round-trip.
/// Every other case (inherit, fail-open, or a missing summary) falls back to the parent-toolset `inherit` names so the prompt always renders fully.
fn role_tool_names_from(
    override_: &crate::session::goal_planner::RoleSpawnOverride,
    cache: &PanelResolveCache,
    inherit: &crate::session::goal_role_tools::RoleToolNames,
) -> crate::session::goal_role_tools::RoleToolNames {
    use distill_tools::implementations::distill::task::types::SubagentDescribeOutcome;
    // `override_.agent_type` is the committed harness; the cache is keyed on it.
    match override_.agent_type.as_deref() {
        Some(harness) => match cache.describe.get(harness) {
            Some(SubagentDescribeOutcome::Ok(summary)) => {
                crate::session::goal_role_tools::RoleToolNames::from_summary(summary)
            }
            _ => inherit.clone(),
        },
        None => inherit.clone(),
    }
}

/// How [`SessionActor::record_verdict_on_orchestration`] updates the orchestration's `last_classifier_gaps`.
/// A real `NotAchieved` panel result stamps fresh curated gaps (`Set`), and a verdict that resolves them clears (`Clear`).
/// A synthetic verdict that ran no panel leaves any stored real gaps replaying into the continuation directive (`Preserve`).
pub(crate) enum GapsUpdate<'a> {
    Set(&'a str),
    Clear,
    Preserve,
}

/// Consecutive goal evaluations without observable progress before the goal
/// is told to change approach.
pub(crate) const GOAL_ESCALATE_AFTER_STALLED_EVALUATIONS: u32 = 2;

/// How a paused goal takes the user's answer.
const GOAL_DECISION_HINT: &str = "Type /goal resume <instruction> to continue with your decision (for example, the option to take), or /goal resume once it is resolved.";

impl SessionActor {
    /// The worktree the goal delivers into: the evaluator's validated
    /// verification target, else the session directory.
    pub(super) async fn goal_delivery_root(&self) -> std::path::PathBuf {
        let target = self
            .goal_tracker
            .lock()
            .snapshot()
            .and_then(|goal| goal.progress.verification_target.clone());
        if let Some(target) = target
            && crate::session::goal_classifier::evidence::validate_verification_target(&target)
                .await
                .is_ok()
        {
            return std::path::PathBuf::from(target.workspace_root);
        }
        self.tool_context.cwd.as_path().to_path_buf()
    }

    /// What the harness itself observed about the delivery: the worktree state
    /// and the checks this request finished.
    async fn goal_work_signals(&self) -> crate::session::goal_evaluator::GoalWorkSignals {
        use crate::session::goal_evaluator::{GoalCheckOutcome, GoalWorkSignals};
        let root = self.goal_delivery_root().await;
        let workspace = crate::session::goal_classifier::evidence::workspace_state(&root).await;
        let fingerprint = workspace.as_ref().map(|state| state.fingerprint.as_str());
        let checks = self
            .jev_ledger
            .borrow()
            .facts
            .recorded_checks()
            .iter()
            .map(|check| {
                GoalCheckOutcome::new(
                    &check.command,
                    &check.command_hash,
                    &check.cwd,
                    check.failed,
                    &check.excerpt,
                    fingerprint,
                )
            })
            .collect();
        GoalWorkSignals {
            delivery_root: root.display().to_string(),
            workspace,
            checks,
        }
    }

    /// One progress evaluation over `signals`, on the main model. A
    /// checkpoint the harness settles makes no call at all (see
    /// [`Self::skip_goal_checkpoint`]); what is left needs main's judgment: a
    /// pending blocker main may confirm, or a round where only main can tell
    /// progress from a stall.
    async fn evaluate_goal_round(
        &self,
        checkpoint: bool,
        signals: &crate::session::goal_evaluator::GoalWorkSignals,
    ) -> Result<crate::session::goal_evaluator::GoalEvaluatorVerdict, String> {
        use crate::session::goal_evaluator::{
            bounded_goal_transcript, build_goal_evaluator_request, goal_evaluator_effort,
            parse_goal_evaluator_verdict,
        };
        let (objective, plan_file, progress, prior_gaps, seen_work, credited_work) = {
            let tracker = self.goal_tracker.lock();
            let snapshot = tracker
                .snapshot()
                .ok_or_else(|| "goal state disappeared before evaluation".to_string())?;
            (
                snapshot.objective.clone(),
                snapshot.plan_file.clone(),
                snapshot.progress.clone(),
                snapshot.last_classifier_gaps.clone(),
                snapshot.seen_work.clone(),
                snapshot.credited_work.clone(),
            )
        };
        let harness_observed = seen_work.harness_observed(signals);
        let harness_progress = seen_work.has_new_work_beyond(&credited_work, signals);
        let resolved_skills = self.goal_evaluator_skill_context(&objective).await;
        let transcript = bounded_goal_transcript(&self.chat_state_handle.get_conversation().await);
        let plan = match plan_file {
            Some(path) => tokio::fs::read_to_string(path)
                .await
                .ok()
                .map(|text| distill_tools::util::truncate_str(&text, 16 * 1024).to_owned()),
            None => None,
        };
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let session_effort = sampling_config
            .as_ref()
            .and_then(|config| config.reasoning_effort);
        let active_model = sampling_config
            .map(|config| config.model)
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| self.models_manager.current_model_id().0.to_string());
        let session_id = self.session_info.id.to_string();
        let model = active_model;
        let supported_efforts: Vec<_> = self
            .models_manager
            .model_reasoning_efforts(&model)
            .into_iter()
            .map(|option| option.value)
            .collect();
        let effort = goal_evaluator_effort(session_effort, &supported_efforts);
        let mut last_error = String::new();
        // One retry on the session model before a failed evaluation pauses the goal.
        for _ in 0..2 {
            let client = match self.prepare_chat_completion(false).await {
                Ok(client) => client,
                Err(error) => {
                    last_error = format!("could not prepare evaluator client: {error}");
                    continue;
                }
            };
            let mut request = build_goal_evaluator_request(
                &objective,
                &transcript,
                plan.as_deref(),
                model.clone(),
                &session_id,
                &progress,
                &resolved_skills,
                prior_gaps.as_deref(),
                &harness_observed,
                effort,
            );
            if checkpoint {
                request.items.push(ConversationItem::user(
                    "This is an intermediate progress checkpoint while the Worker is still executing. Assess only material progress and repeated work. Ongoing work is not itself a blocker. Do not request completion or verifier rechecks; return continue or blocked with the next concrete step.",
                ));
            }
            let attempt = super::side_call::auxiliary_attempt(&client, &request);
            let started = std::time::Instant::now();
            let response = match client.conversation_collect(request).await {
                Ok(response) => response,
                Err(error) => {
                    super::side_call::record_auxiliary_failures(
                        self,
                        std::slice::from_ref(&attempt),
                        true,
                    );
                    let _ = self
                        .chat_state_handle
                        .mark_usage_incomplete(true, true)
                        .await;
                    last_error = format!("goal evaluator request failed: {error}");
                    continue;
                }
            };
            super::side_call::record_auxiliary_response(
                self,
                if checkpoint {
                    "goal_progress_checkpoint"
                } else {
                    "goal_evaluator"
                },
                &model,
                &attempt,
                &response,
                Some(started.elapsed().as_millis() as u64),
                true,
            );
            if response.usage.is_none() {
                let _ = self
                    .chat_state_handle
                    .mark_usage_incomplete(true, true)
                    .await;
                last_error = "goal evaluator response omitted usage accounting".to_string();
                continue;
            }
            match parse_goal_evaluator_verdict(&response.assistant_text()) {
                Ok(verdict) => {
                    if let Some(target) = &verdict.verification_target {
                        if let Err(error) =
                            crate::session::goal_classifier::evidence::validate_verification_target(
                                target,
                            )
                            .await
                        {
                            last_error = error;
                            continue;
                        }
                    }
                    match progress.clone().record(&verdict, harness_progress) {
                        Ok(()) => return Ok(verdict),
                        Err(error) => last_error = error,
                    }
                }
                Err(error) => last_error = error.to_string(),
            }
        }
        Err(last_error)
    }

    /// The utility lane the goal roles use; `None` keeps each on its main path.
    pub(super) async fn goal_utility_lane(&self) -> Option<crate::jev_cheap::CheapLane> {
        #[cfg(test)]
        return crate::session::goal_evaluator::test_goal_utility_config()
            .and_then(|config| crate::jev_cheap::CheapLane::from_sampler_config(&config));
        #[cfg(not(test))]
        self.cheap_lane(distill_workspace::jev::flags::JevLever::ECheapCompress)
            .await
    }

    /// The utility model as a sampler, for a verdict no closed utility task
    /// covers (the laziness classifier). A closed lane is
    /// rebuilt on its own endpoint; one that cannot be leaves the caller on
    /// the main model.
    pub(super) async fn goal_utility_sampler(
        &self,
    ) -> Option<(distill_sampler::SamplingClient, String)> {
        #[cfg(test)]
        {
            let config = crate::session::goal_evaluator::test_goal_utility_config()?;
            let model = config.model.clone();
            return Some((distill_sampler::SamplingClient::new(config).ok()?, model));
        }
        #[cfg(not(test))]
        {
            let lane = self
                .cheap_lane(distill_workspace::jev::flags::JevLever::ECheapCompress)
                .await?;
            let model = lane.model().to_owned();
            let base_url = match &lane.transport {
                crate::jev_cheap::UtilityTransport::Sampler(sampler) => {
                    return Some((sampler.client().clone(), model));
                }
                crate::jev_cheap::UtilityTransport::Closed(client) => {
                    client.config().base_url.clone()
                }
            };
            let catalog = self.models_manager.models();
            let mut config = if crate::agent::config::find_model_by_id(&catalog, &model).is_some() {
                let creds = self.chat_state_handle.get_credentials().await;
                self.aux_sampler_config_with(&model, &creds)?
            } else {
                // A raw OpenRouter priority list: the sampler sends one model,
                // so the checkpoint uses the list's first.
                let first = model.split(',').map(str::trim).find(|id| !id.is_empty())?;
                crate::jev_cheap::CheapLane::standalone_sampler_config(first)?
            };
            let model = config.model.clone();
            if config.base_url.trim_end_matches('/') != base_url {
                return None;
            }
            config.reasoning_effort = self
                .models_manager
                .model_reasoning_efforts(&config.model)
                .into_iter()
                .min_by_key(|option| crate::session::acp_session::effort_rank(option.value))
                .map(|option| option.value);
            Some((distill_sampler::SamplingClient::new(config).ok()?, model))
        }
    }

    /// The evaluator's `resolved_skills`: each pinned skill's utility excerpt,
    /// chosen once per goal and kept in goal state so the goal message stays
    /// the same every round; the full body when there is no usable excerpt.
    async fn goal_evaluator_skill_context(&self, objective: &str) -> String {
        use crate::session::goal_evaluator::{
            GoalSkillExcerpt, pinned_goal_skills, render_goal_skills, skill_body_hash,
            utility_skill_excerpt,
        };
        let skills = self.slash_skills_for_resolve().await;
        let pinned = pinned_goal_skills(objective, &skills).await;
        let mut excerpts = self
            .goal_tracker
            .lock()
            .snapshot()
            .map(|goal| goal.skill_excerpts.clone())
            .unwrap_or_default();
        let mut lane = None;
        let mut chosen = false;
        for skill in &pinned {
            let Ok(body) = &skill.body else {
                continue;
            };
            let body_hash = skill_body_hash(body);
            if excerpts
                .iter()
                .any(|e| e.path == skill.path && e.body_hash == body_hash)
            {
                continue;
            }
            if lane.is_none() {
                lane = Some(self.goal_utility_lane().await);
            }
            // No lane: nothing is recorded, so the full body goes out as before.
            let Some(Some(lane)) = &lane else {
                break;
            };
            let excerpt = utility_skill_excerpt(lane, body).await;
            excerpts.retain(|e| e.path != skill.path);
            excerpts.push(GoalSkillExcerpt {
                path: skill.path.clone(),
                body_hash,
                excerpt,
            });
            chosen = true;
        }
        if chosen {
            let mut tracker = self.goal_tracker.lock();
            if let Some(goal) = tracker.snapshot_mut() {
                goal.skill_excerpts = excerpts.clone();
                self.goal_notify_sender().persist_goal_state(&tracker);
            }
        }
        render_goal_skills(&pinned, &excerpts)
    }

    fn record_goal_round_progress(&self, detail: &str, failed: bool) {
        let detail = distill_tools::util::truncate_str(detail.trim(), 500).to_owned();
        let mut tracker = self.goal_tracker.lock();
        let round = tracker.snapshot_mut().map(|snapshot| {
            snapshot.total_worker_rounds = snapshot.total_worker_rounds.saturating_add(1);
            snapshot.total_worker_rounds
        });
        let mut entry = crate::session::goal_tracker::GoalHistoryEntry::now(
            if failed {
                crate::session::goal_tracker::GoalEvent::WorkerFailed
            } else {
                crate::session::goal_tracker::GoalEvent::WorkerCompleted
            },
            (!detail.is_empty()).then_some(detail),
        );
        entry.round = round;
        tracker.append_history(entry);
    }

    pub(super) async fn verify_goal_candidate(&self, recheck_only: bool) {
        use crate::session::goal_classifier::GoalClassifierOutcome;
        let policy = self.resolve_goal_classifier_policy();
        if !policy.enabled {
            self.auto_pause_goal_if_active_with_message(
                crate::session::goal_tracker::GoalPauseReason::Infra,
                "Goal verification is unavailable. Resume after enabling the verifier.".to_string(),
            )
            .await;
            return;
        }
        let attempts = self
            .goal_tracker
            .lock()
            .snapshot()
            .map_or(0, |snapshot| snapshot.classifier_runs_attempted);
        if attempts >= policy.max_runs {
            self.auto_pause_for_classifier_cap(attempts, "", "").await;
            return;
        }
        let Some(attempt) = self.reserve_classifier_attempt_slot(&policy) else {
            return;
        };
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        self.emit_goal_verifying(current_tokens);
        let outcome = {
            let _latch = TrackerDropGuard::new(&self.goal_tracker, |tracker| {
                if let Some(snapshot) = tracker.snapshot_mut() {
                    snapshot.verifying_in_flight = false;
                }
            });
            self.run_verification_stage_for_drain(attempt, policy.max_runs, recheck_only)
                .await
        };
        if self.goal_tracker.lock().status()
            != Some(crate::session::goal_tracker::GoalStatus::Active)
        {
            return;
        }
        if let GoalClassifierOutcome::FailOpenAchieved { reason, .. } = outcome {
            self.goal_tracker.lock().rollback_classifier_attempt();
            self.auto_pause_goal_if_active_with_message(
                crate::session::goal_tracker::GoalPauseReason::Infra,
                format!(
                    "Goal verification infrastructure failed ({}). Run /goal resume to retry.",
                    reason.as_const_str()
                ),
            )
            .await;
            return;
        }
        let notify = self.goal_notify_sender();
        if recheck_only {
            if let GoalClassifierOutcome::Achieved { details_path } = &outcome {
                let (tokens_used, finished) = self.goal_tokens(current_tokens);
                let mut tracker = self.goal_tracker.lock();
                Self::record_verdict_on_orchestration(
                    &mut tracker,
                    crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved,
                    Some(details_path),
                    GapsUpdate::Clear,
                );
                tracker.reset_classifier_stall();
                tracker.reset_strategist_state();
                if let Some(goal) = tracker.snapshot_mut() {
                    goal.progress.no_progress_rounds = 0;
                }
                notify.emit_goal_updated(&mut tracker, tokens_used, finished);
                return;
            }
        }
        self.apply_classifier_outcome(&policy, attempt, outcome, &notify)
            .await;
    }

    fn reserve_classifier_attempt_slot(&self, policy: &GoalClassifierPolicy) -> Option<u32> {
        let mut tracker = self.goal_tracker.lock();
        let snapshot = tracker.snapshot_mut()?;
        snapshot.classifier_runs_attempted = snapshot.classifier_runs_attempted.saturating_add(1);
        snapshot.classifier_max_runs = Some(policy.max_runs);
        snapshot.rounds_since_verify = 0;
        Some(snapshot.classifier_runs_attempted)
    }

    pub(super) async fn apply_classifier_outcome(
        &self,
        policy: &GoalClassifierPolicy,
        attempt: u32,
        outcome: crate::session::goal_classifier::GoalClassifierOutcome,
        notify: &crate::session::goal_orchestrator::GoalNotifySender,
    ) {
        use crate::session::goal_classifier::GoalClassifierOutcome;
        use crate::session::goal_tracker::GoalClassifierVerdict;
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished) = self.goal_tokens(current_tokens);
        match outcome {
            GoalClassifierOutcome::Achieved { details_path } => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::Achieved,
                        Some(&details_path),
                        GapsUpdate::Clear,
                    );
                    tracker.reset_strategist_state();
                    tracker.complete();
                    notify.emit_goal_updated(&mut tracker, tokens_used, finished);
                }
                self.maybe_run_goal_summarizer(attempt).await;
            }
            GoalClassifierOutcome::NotAchieved {
                details_path,
                gaps_summary,
                pause_summary,
                gap_fingerprint,
            } => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::NotAchieved,
                        Some(&details_path),
                        GapsUpdate::Set(&gaps_summary),
                    );
                    tracker.record_not_achieved_streak();
                }
                if attempt >= policy.max_runs {
                    self.auto_pause_for_classifier_cap(attempt, &details_path, &pause_summary)
                        .await;
                    return;
                }
                let stalled = !gap_fingerprint.is_empty()
                    && self
                        .goal_tracker
                        .lock()
                        .record_classifier_stall(&gap_fingerprint);
                if stalled {
                    self.escalate_classifier_stall(attempt, &gaps_summary).await;
                } else {
                    let every = self.goal_strategist_every;
                    let claimed =
                        self.goal_tracker
                            .lock()
                            .claim_strategist_fire(|consecutive, last| {
                                crate::session::goal_strategist::strategist_should_fire(
                                    consecutive,
                                    last,
                                    every,
                                )
                            });
                    if let Some(consecutive) = claimed {
                        self.maybe_run_goal_strategist(attempt, consecutive).await;
                    }
                }
                notify.emit_goal_updated(&mut self.goal_tracker.lock(), tokens_used, finished);
            }
            GoalClassifierOutcome::Blocked {
                details_path,
                pause_summary,
            } => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::NotAchieved,
                        Some(&details_path),
                        GapsUpdate::Clear,
                    );
                    tracker.rollback_classifier_attempt();
                    tracker.reset_classifier_stall();
                    tracker.reset_strategist_state();
                }
                self.auto_pause_for_classifier_blocked(&details_path, &pause_summary)
                    .await;
            }
            GoalClassifierOutcome::FailOpenAchieved { .. } => {
                unreachable!("infra outcomes are handled fail-closed before apply")
            }
        }
    }

    pub(super) fn record_verdict_on_orchestration(
        tracker: &mut crate::session::goal_tracker::GoalTracker,
        verdict: crate::session::goal_tracker::GoalClassifierVerdict,
        details_path: Option<&str>,
        gaps: GapsUpdate<'_>,
    ) {
        if let Some(o) = tracker.snapshot_mut() {
            o.last_classifier_verdict = Some(verdict);
            if let Some(p) = details_path {
                o.last_classifier_details_path = Some(p.to_string());
            }
            match gaps {
                GapsUpdate::Set(g) => o.last_classifier_gaps = Some(g.to_string()),
                GapsUpdate::Clear => o.last_classifier_gaps = None,
                GapsUpdate::Preserve => {}
            }
            o.last_classifier_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    pub(super) fn resolve_goal_classifier_policy(&self) -> GoalClassifierPolicy {
        let bonus = self
            .goal_tracker
            .lock()
            .snapshot()
            .map_or(0, |o| o.strategist_cap_bonus);
        GoalClassifierPolicy {
            enabled: self.goal_classifier_enabled,
            max_runs: self.goal_classifier_max_runs.saturating_add(bonus),
        }
    }

    fn emit_goal_verifying(&self, current_tokens: i64) {
        if !self.goal_harness_enabled() {
            return;
        }
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let update = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            let Some(o) = tracker.snapshot_mut() else {
                return;
            };
            o.verifying_in_flight = true;
            crate::session::goal_orchestrator::build_goal_updated(o, tokens_used, finished_marginal)
        };
        self.goal_notify_sender().send_update(update);
    }

    pub(super) fn emit_goal_planning(&self, current_tokens: i64) {
        if !self.goal_harness_enabled() {
            return;
        }
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let update = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            let Some(o) = tracker.snapshot_mut() else {
                return;
            };
            o.planning_in_flight = true;
            crate::session::goal_orchestrator::build_goal_updated(o, tokens_used, finished_marginal)
        };
        self.goal_notify_sender().send_update(update);
    }

    async fn auto_pause_for_classifier_cap(
        &self,
        attempt: u32,
        details_path: &str,
        pause_summary: &str,
    ) {
        self.events
            .emit(crate::session::events::Event::GoalClassifierCapReached { attempt });
        let msg = format_goal_pause_message(
            &format!("Goal classifier rejected completion {attempt} times — goal auto-paused."),
            pause_summary,
            details_path,
        );
        self.auto_pause_goal_if_active_with_message(
            crate::session::goal_tracker::GoalPauseReason::BackOff,
            msg,
        )
        .await;
    }

    /// The verifier rejected completion with the same gaps again. A goal does
    /// not pause for that: the strategist restructures the approach and the
    /// goal is told to change approach on the gaps.
    async fn escalate_classifier_stall(&self, attempt: u32, gaps_summary: &str) {
        let claimed = self.goal_tracker.lock().claim_strategist_fire(|_, _| true);
        if let Some(consecutive) = claimed {
            self.maybe_run_goal_strategist(attempt, consecutive).await;
        }
        let rejections = self
            .goal_tracker
            .lock()
            .snapshot()
            .map_or(0, |goal| goal.consecutive_not_achieved);
        self.run_goal_escalation(
            &format!("The verifier rejected completion with the same gaps again:\n{gaps_summary}"),
            "Resolve these verifier gaps with evidence the verifier can audit.",
            rejections,
        )
        .await;
    }

    async fn auto_pause_for_classifier_blocked(&self, details_path: &str, pause_summary: &str) {
        let msg = format_goal_pause_message(
            "Goal verification found no model-fixable path — paused for your decision.",
            pause_summary,
            details_path,
        );
        self.auto_pause_goal_if_active_with_message(
            crate::session::goal_tracker::GoalPauseReason::Verification,
            msg,
        )
        .await;
    }

    pub(super) async fn run_verification_stage_for_drain(
        &self,
        attempt: u32,
        max_runs: u32,
        recheck_only: bool,
    ) -> crate::session::goal_classifier::GoalClassifierOutcome {
        use crate::session::events::GoalClassifierFailOpenReason;
        use crate::session::goal_classifier::{
            ChannelSpawner, GoalClassifierOutcome, VerificationStageInputs, run_verification_stage,
        };

        let (
            objective,
            verifier_id,
            baseline_commit,
            plan_file,
            plan_baseline_file,
            goal_created_at,
            prior_skeptic0,
            prior_gaps,
            first_final_response,
            scratch_dir_ready,
            verification_target,
        ) = {
            let tracker = self.goal_tracker.lock();
            let Some(o) = tracker.snapshot() else {
                return GoalClassifierOutcome::FailOpenAchieved {
                    reason: GoalClassifierFailOpenReason::GoalNotActiveAtResolve,
                    details_path: String::new(),
                };
            };
            (
                o.objective.clone(),
                o.verifier_id.clone(),
                o.changes_baseline_commit.clone(),
                o.plan_file.clone(),
                o.plan_baseline_file.clone(),
                crate::session::goal_classifier::evidence::parse_created_at_to_unix(&o.created_at),
                o.skeptic0_session_id.clone(),
                o.last_classifier_gaps.clone(),
                o.first_final_response.clone(),
                o.scratch_dir_ready,
                o.progress.verification_target.clone(),
            )
        };

        let workspace_root = verification_target
            .as_ref()
            .map(|target| std::path::Path::new(&target.workspace_root))
            .unwrap_or_else(|| self.tool_context.cwd.as_path());
        let baseline_commit = verification_target
            .as_ref()
            .map(|target| target.baseline_commit.as_str())
            .or(baseline_commit.as_deref());
        if let Some(target) = &verification_target {
            if let Err(error) =
                crate::session::goal_classifier::evidence::validate_verification_target(target)
                    .await
            {
                tracing::warn!(%error, "verification target became unavailable");
                return GoalClassifierOutcome::FailOpenAchieved {
                    reason: GoalClassifierFailOpenReason::SamplerError,
                    details_path: String::new(),
                };
            }
        }
        let review_objective = if recheck_only {
            format!(
                "Independently recheck ONLY the prior findings below against the supplied delivery worktree and current evidence. Report any of those findings that remain unresolved. An all-clear retires these findings only; the harness keeps the overall goal active and will run final verification separately. Do not add unrelated goal criteria to this limited recheck.\n\nPrior findings:\n{}\n\nOriginal goal (context, not a completion claim):\n{objective}",
                prior_gaps.as_deref().unwrap_or_default(),
            )
        } else {
            objective.clone()
        };

        let (mut final_response, anchor_to_persist) = {
            let current = {
                let items = self.chat_state_handle.get_conversation().await;
                crate::session::goal_classifier::evidence::extract_final_response(&items)
                    .unwrap_or_default()
            };
            let composed =
                crate::session::goal_classifier::evidence::compose_verifier_final_response(
                    first_final_response.as_deref(),
                    current,
                );
            (composed.to_send, composed.to_persist)
        };
        if let Some(goal) = self.goal_tracker.lock().snapshot() {
            final_response = format!(
                "Persisted criteria/evidence references to audit (not proof by themselves):\n{}\n\n{final_response}",
                serde_json::to_string(&goal.progress.criteria).unwrap_or_default(),
            );
        }
        final_response = format!(
            "Delivery worktree: {}\nRecorded baseline: {}\nChanged-file paths are relative to this worktree.\n\n{final_response}",
            workspace_root.display(),
            baseline_commit.unwrap_or("(not recorded)"),
        );

        let model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();

        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            tracing::warn!("verification stage: no subagent coordinator channel; failing open");
            return GoalClassifierOutcome::FailOpenAchieved {
                reason: GoalClassifierFailOpenReason::SamplerError,
                details_path: String::new(),
            };
        };
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        let task_tool_name = self.resolve_goal_tool_names().await.task;

        let skeptic_count = if recheck_only {
            1
        } else {
            self.goal_tracker
                .lock()
                .snapshot()
                .map_or(self.goal_verifier_skeptic_count, |goal| {
                    if !goal.progress.criteria.is_empty() && !goal.progress.needs_review_panel {
                        1
                    } else {
                        self.goal_verifier_skeptic_count
                    }
                })
        };
        let n = skeptic_count.clamp(
            crate::session::goal_classifier::GOAL_VERIFIER_SKEPTIC_MIN,
            crate::session::goal_classifier::GOAL_VERIFIER_SKEPTIC_MAX,
        );
        let inherit_tool_names = self.resolve_inherit_role_tool_names().await;
        let (skeptic_overrides, skeptic_tool_names): (
            Vec<crate::session::goal_planner::RoleSpawnOverride>,
            Vec<crate::session::goal_role_tools::RoleToolNames>,
        ) = if self.goal_use_current_model_only {
            (
                vec![crate::session::goal_planner::RoleSpawnOverride::default(); n as usize],
                vec![inherit_tool_names.clone(); n as usize],
            )
        } else {
            let assignment = {
                let mut tracker = self.goal_tracker.lock();
                match tracker.snapshot_mut() {
                    Some(o) => {
                        let expanded = crate::session::goal_classifier::expand_skeptic_assignment(
                            &o.skeptic_model_assignment,
                            &self.goal_role_models.skeptic_pool,
                            n as usize,
                        );
                        o.skeptic_model_assignment = expanded.clone();
                        expanded
                    }
                    None => Vec::new(),
                }
            };
            let available_models = self.models_manager.models();
            let mut cache = PanelResolveCache::default();
            let mut overrides = Vec::with_capacity(n as usize);
            for idx in 0..n {
                let ov = match assignment.get(idx as usize) {
                    Some(pair) => {
                        self.resolve_goal_role_override(
                            "skeptic",
                            Some(idx),
                            pair,
                            RoleCapability::Skeptic,
                            &event_tx,
                            &available_models,
                            &mut cache,
                        )
                        .await
                    }
                    None => crate::session::goal_planner::RoleSpawnOverride::default(),
                };
                overrides.push(ov);
            }
            let tool_names = overrides
                .iter()
                .map(|ov| role_tool_names_from(ov, &cache, &inherit_tool_names))
                .collect();
            (overrides, tool_names)
        };

        let spawner: std::sync::Arc<dyn crate::session::goal_classifier::GoalClassifierSpawner> =
            std::sync::Arc::new(ChannelSpawner {
                event_tx,
                foreground_wait: Some(crate::tools::tool_context::subagent_foreground_wait(
                    self.tool_context.blocking_wait_depth.clone(),
                )),
                parent_session_id: self.session_id_string(),
                parent_prompt_id,
                cwd: Some(workspace_root.to_string_lossy().into_owned()),
                trace_sink: Some((self.chat_state_handle.clone(), task_tool_name)),
                skeptic_overrides,
                events: Some(self.events.writer()),
            });

        let implementer_scratch =
            crate::session::goal_tracker::implementer_scratch_dir(&verifier_id);
        // The harness's own record of the checks this goal ran, so skeptics
        // audit it instead of asking the implementer for log files.
        let harness_checks: Vec<crate::session::goal_classifier::HarnessCheck> = self
            .jev_ledger
            .borrow()
            .facts
            .recorded_checks()
            .iter()
            .map(|check| crate::session::goal_classifier::HarnessCheck {
                command: check.command.clone(),
                passed: !check.failed,
                excerpt: check.excerpt.clone(),
                finished_at: check.finished_at,
            })
            .collect();

        let inputs = VerificationStageInputs {
            objective: &review_objective,
            final_response: &final_response,
            baseline_commit,
            workspace_root,
            verifier_id: &verifier_id,
            attempt,
            model_id: &model_id,
            goal_created_at,
            plan_file: if recheck_only {
                None
            } else {
                plan_file.as_deref()
            },
            plan_baseline_file: if recheck_only {
                None
            } else {
                plan_baseline_file.as_deref()
            },
            implementer_scratch_dir: implementer_scratch.as_path(),
            scratch_dir_ready,
            skeptic_count,
            max_runs,
            prior_skeptic0_session_id: if recheck_only {
                None
            } else {
                prior_skeptic0.as_deref()
            },
            prior_gaps: prior_gaps.as_deref(),
            tool_names: &skeptic_tool_names,
            inherit_tool_names: &inherit_tool_names,
            harness_checks: &harness_checks,
        };

        let result = run_verification_stage(spawner, inputs, &|e| self.events.emit(e)).await;
        self.chat_state_handle.flush_harness_trace_turn();
        if result.panel_ran
            && let Some(o) = self.goal_tracker.lock().snapshot_mut()
        {
            o.skeptic0_session_id = result.skeptic0_session_id;
            if let Some(anchor) = anchor_to_persist {
                o.first_final_response = Some(anchor);
            }
        }
        result.outcome
    }

    pub(crate) async fn resolve_goal_single_role_override(
        &self,
        role: &'static str,
        choice: &crate::agent::config::GoalRoleModelChoice,
        capability: RoleCapability,
        event_tx: &tokio::sync::mpsc::UnboundedSender<
            distill_tools::implementations::distill::task::types::SubagentEvent,
        >,
    ) -> (
        crate::session::goal_planner::RoleSpawnOverride,
        crate::session::goal_role_tools::RoleToolNames,
        crate::session::goal_role_tools::RoleToolNames,
    ) {
        let inherit = self.resolve_inherit_role_tool_names().await;
        let crate::agent::config::GoalRoleModelChoice::Explicit(pair) = choice else {
            return (
                crate::session::goal_planner::RoleSpawnOverride::default(),
                inherit.clone(),
                inherit,
            );
        };
        let available_models = self.models_manager.models();
        let mut cache = PanelResolveCache::default();
        let override_ = self
            .resolve_goal_role_override(
                role,
                None,
                pair,
                capability,
                event_tx,
                &available_models,
                &mut cache,
            )
            .await;
        let tool_names = role_tool_names_from(&override_, &cache, &inherit);
        (override_, tool_names, inherit)
    }

    pub(crate) async fn resolve_goal_role_override(
        &self,
        role: &'static str,
        skeptic_idx: Option<u32>,
        pair: &crate::util::config::GoalRoleModel,
        capability: RoleCapability,
        event_tx: &tokio::sync::mpsc::UnboundedSender<
            distill_tools::implementations::distill::task::types::SubagentEvent,
        >,
        available_models: &indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
        cache: &mut PanelResolveCache,
    ) -> crate::session::goal_planner::RoleSpawnOverride {
        use crate::session::events::{Event, GoalRoleModelFailOpenReason as Reason};
        use crate::session::goal_planner::RoleSpawnOverride;
        use distill_tools::implementations::distill::task::backend::{
            ChannelBackend, SubagentBackend,
        };
        use distill_tools::implementations::distill::task::types::SubagentDescribeOutcome;

        let fail_open = |reason: Reason| {
            self.emit_event(Event::GoalRoleModelFailOpen {
                role,
                skeptic_idx,
                reason: reason.as_const_str(),
            });
            RoleSpawnOverride::default()
        };

        let Some(entry) = crate::agent::config::find_model_by_id(available_models, &pair.model)
        else {
            return fail_open(Reason::ModelUnknown);
        };
        if !entry.info.user_selectable {
            return fail_open(Reason::ModelUnauthorized);
        }
        if distill_agent::config::is_strict_harness_agent_type(&pair.agent_type)
            && !crate::agent::subagent::subagent_harness_flavor_is_representable(&pair.agent_type)
        {
            return fail_open(Reason::HarnessFlavorUnsupported);
        }
        if !cache.describe.contains_key(&pair.agent_type) {
            let backend = ChannelBackend::new(event_tx.clone());
            let parent_session_id = self.session_id_string();
            let outcome = backend
                .describe_subagent_type(
                    crate::session::goal_planner::GOAL_ROLE_SUBAGENT_TYPE,
                    Some(&pair.agent_type),
                    &parent_session_id,
                )
                .await;
            if matches!(outcome, SubagentDescribeOutcome::Unavailable) {
                return fail_open(Reason::ToolsetUnavailable);
            }
            cache.describe.insert(pair.agent_type.clone(), outcome);
        }
        let summary = match cache.describe.get(&pair.agent_type) {
            Some(SubagentDescribeOutcome::Ok(summary)) => summary,
            Some(SubagentDescribeOutcome::Unknown { .. }) => {
                return fail_open(Reason::ToolsetUnknown);
            }
            Some(SubagentDescribeOutcome::NotAllowed { .. }) => {
                return fail_open(Reason::ToolsetNotAllowed);
            }
            Some(SubagentDescribeOutcome::Disabled) => return fail_open(Reason::ToolsetDisabled),
            Some(SubagentDescribeOutcome::Unavailable) => {
                return fail_open(Reason::ToolsetUnavailable);
            }
            _ => return fail_open(Reason::ToolsetUnavailable),
        };
        if !capability.is_satisfied(summary) {
            return fail_open(Reason::ToolsetIncapable);
        }

        self.emit_event(Event::GoalRoleModelResolved {
            role,
            skeptic_idx,
            model_id: pair.model.clone(),
            agent_type: pair.agent_type.clone(),
            source: "remote",
        });
        RoleSpawnOverride {
            model: Some(pair.model.clone()),
            agent_type: Some(pair.agent_type.clone()),
        }
    }

    pub(super) async fn resolve_goal_tool_names(&self) -> GoalToolNames {
        use distill_tools::types::tool::ToolKind;
        let bridge = self.agent.borrow().tool_bridge().clone();
        GoalToolNames {
            goal: bridge
                .tool_for_kind(ToolKind::GoalUpdate)
                .await
                .unwrap_or_else(|| "update_goal".into()),
            task: bridge
                .tool_for_kind(ToolKind::Task)
                .await
                .unwrap_or_else(|| "spawn_subagent".into()),
            todo: bridge
                .tool_for_kind(ToolKind::Plan)
                .await
                .unwrap_or_else(|| "todo_write".into()),
        }
    }

    pub(crate) async fn resolve_inherit_role_tool_names(
        &self,
    ) -> crate::session::goal_role_tools::RoleToolNames {
        use distill_tools::types::tool::ToolKind;
        let bridge = self.agent.borrow().tool_bridge().clone();
        crate::session::goal_role_tools::RoleToolNames::from_parent(
            bridge.tool_for_kind(ToolKind::Read).await,
            bridge.tool_for_kind(ToolKind::ListDir).await,
            bridge.tool_for_kind(ToolKind::Search).await,
            bridge.tool_for_kind(ToolKind::Write).await,
            bridge.tool_for_kind(ToolKind::Edit).await,
            bridge.tool_for_kind(ToolKind::Execute).await,
            bridge.tool_for_kind(ToolKind::WebSearch).await,
            bridge.tool_for_kind(ToolKind::WebFetch).await,
        )
    }

    pub(super) async fn setup_goal(
        &self,
        objective: &str,
        token_budget: Option<i64>,
    ) -> GoalSetupOutcome {
        let goal_id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().to_rfc3339();
        let token_baseline = self.chat_state_handle.get_total_tokens().await as i64;
        let baseline_commit =
            crate::session::goal_classifier::capture_git_baseline(self.tool_context.cwd.as_path())
                .await;
        self.goal_tracker.lock().create_goal(
            goal_id,
            objective.to_owned(),
            token_budget,
            token_baseline,
            created_at,
            baseline_commit,
        );
        self.goal_turn_task_ids.lock().clear();
        self.clear_pending_classifier_completions();
        self.goal_continuation_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.goal_blocked_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);

        {
            let (tokens_used, finished_marginal) = self.goal_tokens(token_baseline);
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }

        self.maybe_run_goal_planner(objective).await;
        if let Some(msg) = self.planner_pause_short_circuit_message("Goal paused.") {
            return GoalSetupOutcome::Message(msg);
        }

        let names = self.resolve_goal_tool_names().await;
        let planner_enabled = self.goal_planner_enabled;
        let body = {
            let tracker = self.goal_tracker.lock();
            tracker.snapshot().map(|o| {
                let plan_path = goal_reminder_plan_path(planner_enabled, o);
                let scratch_dir =
                    crate::session::goal_tracker::implementer_scratch_dir(&o.verifier_id);
                let scratch = scratch_dir.to_string_lossy();
                if self.goal_runs_on_workflow_engine() {
                    render_goal_rules(
                        objective,
                        &names,
                        "",
                        "",
                        plan_path,
                        &scratch,
                        o.scratch_dir_ready,
                    )
                } else {
                    render_goal_rules_legacy(
                        objective,
                        &names,
                        "",
                        "",
                        plan_path,
                        &scratch,
                        o.scratch_dir_ready,
                    )
                }
            })
        };
        let Some(body) = body else {
            return GoalSetupOutcome::Message(GOAL_CLEARED_DURING_PLANNING.to_string());
        };
        GoalSetupOutcome::Inference {
            reminder: format!("<system-reminder>\n{body}\nStart now.\n</system-reminder>\n\n"),
        }
    }

    /// `guidance` is the user's decision from `/goal resume <text>`; it joins the
    /// resumed turn as the user's own words.
    pub(super) async fn resume_goal(&self, guidance: Option<&str>) -> GoalResumeOutcome {
        use crate::session::goal_tracker::GoalStatus;
        enum ResumeTransition {
            Resumed {
                previous_status: GoalStatus,
                previous_pause_message: Option<String>,
            },
            Nudge,
            AlreadyComplete,
            BudgetLimited,
            NoGoal,
        }
        let transition = {
            let mut tracker = self.goal_tracker.lock();
            match tracker.status() {
                Some(
                    GoalStatus::UserPaused
                    | GoalStatus::BackOffPaused
                    | GoalStatus::NoProgressPaused
                    | GoalStatus::InfraPaused
                    | GoalStatus::Blocked,
                ) => {
                    let previous_status = tracker.status().unwrap();
                    let previous_pause_message =
                        tracker.snapshot().and_then(|o| o.pause_message.clone());
                    tracker.resume();
                    ResumeTransition::Resumed {
                        previous_status,
                        previous_pause_message,
                    }
                }
                Some(GoalStatus::Active) => ResumeTransition::Nudge,
                Some(GoalStatus::Complete) => ResumeTransition::AlreadyComplete,
                Some(GoalStatus::BudgetLimited) => ResumeTransition::BudgetLimited,
                None => ResumeTransition::NoGoal,
            }
        };
        let was_resumed = matches!(transition, ResumeTransition::Resumed { .. });
        if was_resumed {
            let tracker = self.goal_tracker.lock();
            self.goal_notify_sender().persist_goal_state(&tracker);
        }
        let (user_msg, previous_status, previous_pause_message) = match transition {
            ResumeTransition::Resumed {
                previous_status,
                previous_pause_message,
            } => ("Goal resumed.", previous_status, previous_pause_message),
            ResumeTransition::Nudge => (
                "Goal nudged — refreshing context.",
                GoalStatus::Active,
                None,
            ),
            ResumeTransition::AlreadyComplete => {
                return GoalResumeOutcome::Message(
                    "Goal is already complete. Use /goal <objective> to start a new one."
                        .to_string(),
                );
            }
            ResumeTransition::BudgetLimited => {
                return GoalResumeOutcome::Message(
                    "Goal is budget-limited. Use /goal clear, then /goal <objective>.".to_string(),
                );
            }
            ResumeTransition::NoGoal => {
                return GoalResumeOutcome::Message(
                    "No goal set. Use /goal <objective> to start one.".to_string(),
                );
            }
        };

        self.goal_continuation_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.goal_blocked_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);

        let mut planner_published = false;
        if was_resumed {
            let needs_retry = {
                let tracker = self.goal_tracker.lock();
                tracker
                    .snapshot()
                    .map(|o| o.status == GoalStatus::Active && o.plan_file.is_none())
                    .unwrap_or(false)
            };
            if needs_retry {
                let objective = self
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .map(|o| o.objective.clone());
                if let Some(objective) = objective {
                    self.maybe_run_goal_planner(&objective).await;
                    if let Some(msg) = self
                        .planner_pause_short_circuit_message("Planning failed again; goal paused.")
                    {
                        return GoalResumeOutcome::Message(msg);
                    }
                    // `needs_retry` saw `plan_file == None` and the goal is still Active, so `Some` means this resume published.
                    planner_published = self
                        .goal_tracker
                        .lock()
                        .snapshot()
                        .is_some_and(|o| o.plan_file.is_some());
                }
            }
        }

        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);

        {
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }

        let names = self.resolve_goal_tool_names().await;

        let block_recap = match (previous_status, previous_pause_message.as_deref()) {
            (GoalStatus::Blocked, Some(msg)) => format!(
                "Previous state: Blocked.\n\
                 Previous block reason: {msg}\n\
                 Re-evaluate whether the blocker has been addressed. If yes, continue \
                 working. If no, you may block again with an updated reason.\n\
                 \n"
            ),
            // The pause was the planner's own failure; a plan published on this resume supersedes it.
            (GoalStatus::InfraPaused, _) if planner_published => String::new(),
            (GoalStatus::InfraPaused, Some(msg)) => format!(
                "Previous state: Paused (infrastructure error).\n\
                 Previous error: {msg}\n\
                 The prior turn failed due to infrastructure. Retry when the error \
                 condition is resolved.\n\
                 \n"
            ),
            (GoalStatus::InfraPaused, None) => "Previous state: Paused (infrastructure error).\n\
                 The prior turn failed due to infrastructure. Retry when the error \
                 condition is resolved.\n\
                 \n"
            .to_string(),
            _ => String::new(),
        };

        let planner_enabled = self.goal_planner_enabled;
        let reminder = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            tracker.snapshot().map(|o| {
                let elapsed = crate::session::goal_orchestrator::format_elapsed(o.elapsed_ms);
                let goal_state = format!(
                    "<goal-state>\nStatus: Active\nTokens: {tokens} | Elapsed: \
                     {elapsed}\n</goal-state>\n\n",
                    tokens = tokens_used,
                );
                let plan_path = goal_reminder_plan_path(planner_enabled, o);
                let scratch_dir =
                    crate::session::goal_tracker::implementer_scratch_dir(&o.verifier_id);
                let scratch = scratch_dir.to_string_lossy();
                let body = if self.goal_runs_on_workflow_engine() {
                    render_goal_rules(
                        &o.objective,
                        &names,
                        &block_recap,
                        &goal_state,
                        plan_path,
                        &scratch,
                        o.scratch_dir_ready,
                    )
                } else {
                    render_goal_rules_legacy(
                        &o.objective,
                        &names,
                        &block_recap,
                        &goal_state,
                        plan_path,
                        &scratch,
                        o.scratch_dir_ready,
                    )
                };
                let guidance = guidance
                    .map(str::trim)
                    .filter(|guidance| !guidance.is_empty())
                    .map(|guidance| {
                        format!("\n<goal_resume_guidance>\n{guidance}\n</goal_resume_guidance>")
                    })
                    .unwrap_or_default();
                format!(
                    "<system-reminder>\n{body}\nContinue working now.\n</system-reminder>{guidance}"
                )
            })
        };
        let Some(reminder) = reminder else {
            return GoalResumeOutcome::Message("Goal state is missing.".to_string());
        };
        GoalResumeOutcome::Inference {
            reminder,
            user_msg: user_msg.to_string(),
        }
    }

    /// Seeds compacted `last_user_query` from the live objective.
    /// Paused / blocked / budget-limited goals leave the human prompt in place.
    pub(crate) fn goal_objective_for_compaction(&self) -> Option<String> {
        let tracker = self.goal_tracker.lock();
        let o = tracker.snapshot()?;
        if o.status != crate::session::goal_tracker::GoalStatus::Active {
            return None;
        }
        let objective = o.objective.trim();
        if objective.is_empty() {
            None
        } else {
            Some(o.objective.clone())
        }
    }

    /// Summarizer pin so multi-compact cannot drop the objective.
    pub(crate) fn goal_compaction_user_context(&self) -> Option<String> {
        let (objective, plan_path, evaluator_step) = {
            let tracker = self.goal_tracker.lock();
            let o = tracker.snapshot()?;
            if o.status != crate::session::goal_tracker::GoalStatus::Active {
                return None;
            }
            if o.objective.trim().is_empty() {
                return None;
            }
            (
                o.objective.clone(),
                goal_reminder_plan_path(self.goal_planner_enabled, o)
                    .map(std::path::Path::to_path_buf),
                o.last_evaluator_next_step.clone(),
            )
        };
        let mut ctx = format!(
            "Objective: {objective}\nDo not restart this goal or revive a prior unrelated task. Continue from the current next step."
        );
        if let Some(step) = resolve_goal_next_step(evaluator_step.as_deref(), plan_path.as_deref()) {
            ctx.push_str("\nNext step: ");
            ctx.push_str(&step);
        }
        Some(ctx)
    }

    /// Preserves caller `/compact <text>` when folding in the goal pin.
    pub(crate) fn merge_goal_compaction_user_context(
        &self,
        user_context: Option<String>,
    ) -> Option<String> {
        let Some(goal_ctx) = self.goal_compaction_user_context() else {
            return user_context;
        };
        match user_context {
            Some(existing) if !existing.trim().is_empty() => {
                Some(format!("{existing}\n\n{goal_ctx}"))
            }
            _ => Some(goal_ctx),
        }
    }

    /// Live GoalTracker snapshot for the post-compaction reminder.
    /// `None` when no goal exists or it already completed.
    /// Continuation-style (next step), not create-style `goal_rules`.
    pub(crate) async fn compaction_goal_section(&self) -> Option<String> {
        use crate::session::goal_tracker::GoalStatus;

        let names = self.resolve_goal_tool_names().await;
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, _) = self.goal_tokens(current_tokens);
        let planner_enabled = self.goal_planner_enabled;
        let (objective, status, elapsed, plan_path, is_active, evaluator_step) = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            let o = tracker.snapshot()?;
            if o.status == GoalStatus::Complete {
                return None;
            }
            let elapsed = crate::session::goal_orchestrator::format_elapsed(o.elapsed_ms);
            let is_active = o.status == GoalStatus::Active;
            let status = match o.status {
                GoalStatus::Active => "Active",
                GoalStatus::UserPaused => "Paused",
                GoalStatus::BackOffPaused => "Paused (back off)",
                GoalStatus::NoProgressPaused => "Paused (no progress)",
                GoalStatus::InfraPaused => "Paused (infrastructure error)",
                GoalStatus::Blocked => "Blocked",
                GoalStatus::BudgetLimited => "Budget limited",
                GoalStatus::Complete => unreachable!("complete goals are omitted above"),
            };
            let plan_path =
                goal_reminder_plan_path(planner_enabled, o).map(std::path::Path::to_path_buf);
            (
                o.objective.clone(),
                status,
                elapsed,
                plan_path,
                is_active,
                o.last_evaluator_next_step.clone(),
            )
        };
        let next_step = resolve_goal_next_step(evaluator_step.as_deref(), plan_path.as_deref())
            .unwrap_or_else(|| format!("Check your `{}` list for next steps.", names.todo));
        let body = if is_active {
            format!(
                "Objective: {objective}\nStatus: {status}\nTokens: {tokens_used} | Elapsed: {elapsed}\n\n{GOAL_CONTINUATION_SENTINEL}\n{next_step}\n\nDo not restart this goal or revive a prior unrelated task."
            )
        } else {
            format!(
                "Objective: {objective}\nStatus: {status}\nTokens: {tokens_used} | Elapsed: {elapsed}\n\nA live goal exists. Do not restart it or revive a prior unrelated task."
            )
        };
        Some(format_compaction_goal_section(&body))
    }

    /// Inject GoalSummary so the next sample sees next-step, not create-style rules.
    pub(crate) async fn reseed_active_goal_after_compaction(&self) {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let Some(plan) = self
            .prepare_goal_continuation(current_tokens, GoalContinuationPurpose::Compaction)
            .await
        else {
            return;
        };
        self.inject_goal_continuation_message(plan.directive).await;
    }

    pub(super) async fn prune_prior_goal_continuation_directives(&self) {
        use distill_sampling_types::conversation::SyntheticReason;

        fn is_goal_continuation_directive(item: &ConversationItem) -> bool {
            matches!(
                item,
                ConversationItem::User(u)
                    if u.synthetic_reason == SyntheticReason::GoalSummary
            ) && item.text_content().contains(GOAL_CONTINUATION_SENTINEL)
        }
        // Deleting an earlier directive rewrites history the provider has
        // cached, re-billing everything after it; at a cold moment (a
        // compaction, a model switch, a long idle) that costs nothing extra.
        // Until then the stale directives stay, each superseded by the latest.
        if !self.chat_state_handle.history_is_cold().await {
            return;
        }
        let conv = self.chat_state_handle.get_conversation().await;
        if !conv.iter().any(is_goal_continuation_directive) {
            return;
        }
        let kept: Vec<ConversationItem> = conv
            .into_iter()
            .filter(|item| !is_goal_continuation_directive(item))
            .collect();
        self.chat_state_handle.replace_conversation(kept);
    }

    pub(super) async fn enforce_goal_token_budget(&self, current_tokens: i64) -> bool {
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let Some(budget) = self.goal_tracker.lock().token_budget() else {
            return false;
        };
        if tokens_used < budget {
            return false;
        }
        self.goal_tracker.lock().budget_limit();
        self.clear_pending_classifier_completions();
        let notify = self.goal_notify_sender();
        notify.emit_goal_updated(
            &mut self.goal_tracker.lock(),
            tokens_used,
            finished_marginal,
        );
        self.send_slash_command_output(&format!(
            "Goal token budget reached ({tokens_used} of {budget} tokens) — goal \
             stopped. Use /goal clear, then /goal <objective> to start a new one."
        ))
        .await;
        true
    }

    async fn prepare_goal_continuation(
        &self,
        current_tokens: i64,
        purpose: GoalContinuationPurpose,
    ) -> Option<GoalContinuationPlan> {
        let legacy = !self.goal_runs_on_workflow_engine();
        let apply_turn_end = matches!(purpose, GoalContinuationPurpose::TurnEnd);
        if apply_turn_end && legacy {
            self.drain_goal_updates(current_tokens, DrainPurpose::TurnEnd)
                .await;
        }

        let goal_active = laziness_injection_active(
            self.goal_harness_enabled(),
            self.goal_tracker.lock().status(),
        );
        if !goal_active {
            return None;
        }

        let stop_pattern = if apply_turn_end {
            match self.assistant_text_signals_premature_stop().await {
                Some(pattern) if self.has_pending_goal_todos().await => Some(pattern),
                _ => None,
            }
        } else {
            None
        };

        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);

        if apply_turn_end && self.enforce_goal_token_budget(current_tokens).await {
            return None;
        }

        if apply_turn_end {
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }

        let names = self.resolve_goal_tool_names().await;
        let todo_tool = &names.todo;
        let goal_tool = names.goal.as_str();

        let planner_enabled = self.goal_planner_enabled;
        let (
            objective,
            elapsed,
            plan_pointer,
            verifier_gaps,
            strategist_note,
            strategy_rec,
            plan_path,
            scratch_dir,
            scratch_ready,
            rounds_since_verify,
            refuted,
            evaluator_step,
        ) = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            let o = tracker.snapshot_mut()?;
            if apply_turn_end {
                o.rounds_since_verify = o.rounds_since_verify.saturating_add(1);
            }
            let rounds_since_verify = o.rounds_since_verify;
            let refuted = o.consecutive_not_achieved >= 1;
            let elapsed = crate::session::goal_orchestrator::format_elapsed(o.elapsed_ms);
            let scratch_dir = crate::session::goal_tracker::implementer_scratch_dir(&o.verifier_id)
                .to_string_lossy()
                .into_owned();
            let plan_path = goal_reminder_plan_path(planner_enabled, o).map(Path::to_path_buf);
            let plan_pointer = plan_path
                .as_deref()
                .map(|p| format!("Plan: {}\n\n", p.display()))
                .unwrap_or_default();
            let verifier_gaps = o
                .last_classifier_gaps
                .as_deref()
                .map(|gaps| {
                    if legacy {
                        render_verifier_gaps_block_legacy(gaps, goal_tool)
                    } else {
                        render_verifier_gaps_block(gaps)
                    }
                })
                .unwrap_or_default();
            let strategy_rec = o.last_strategy_recommendation.clone();
            let strategist_note = strategy_rec
                .as_deref()
                .map(|rec| {
                    render_strategist_note(
                        rec,
                        plan_path.as_deref(),
                        o.last_strategy_path.as_deref(),
                    )
                })
                .unwrap_or_default();
            (
                o.objective.clone(),
                elapsed,
                plan_pointer,
                verifier_gaps,
                strategist_note,
                strategy_rec,
                plan_path,
                scratch_dir,
                o.scratch_dir_ready,
                rounds_since_verify,
                refuted,
                o.last_evaluator_next_step.clone(),
            )
        };
        let next_step = resolve_goal_next_step(evaluator_step.as_deref(), plan_path.as_deref())
            .unwrap_or_else(|| format!("Check your `{todo_tool}` list for next steps."));
        let tokens = u64::try_from(tokens_used).unwrap_or(0);
        let bail_preface = if stop_pattern.is_some() {
            GOAL_CONTINUATION_BAIL_PREFACE
        } else {
            ""
        };
        let reverify_block = if legacy {
            render_goal_reverify_block_legacy(
                rounds_since_verify,
                refuted,
                self.goal_reverify_after,
                goal_tool,
            )
        } else {
            render_goal_reverify_block(rounds_since_verify, refuted, self.goal_reverify_after)
        };
        let mut directive = if legacy {
            render_goal_continuation_directive_legacy(
                &objective,
                tokens,
                &elapsed,
                bail_preface,
                &plan_pointer,
                &verifier_gaps,
                &strategist_note,
                &reverify_block,
                &next_step,
                todo_tool,
                goal_tool,
                &scratch_dir,
                scratch_ready,
            )
        } else {
            render_goal_continuation_directive(
                &objective,
                tokens,
                &elapsed,
                bail_preface,
                &plan_pointer,
                &verifier_gaps,
                &strategist_note,
                &reverify_block,
                &next_step,
                todo_tool,
                &scratch_dir,
                scratch_ready,
            )
        };
        if let Some(goal) = self.goal_tracker.lock().snapshot() {
            directive.push_str("\nPersisted criteria and evidence (audit these sources; recheck only invalidated proofs):\n");
            directive.push_str(&serde_json::to_string(&goal.progress).unwrap_or_default());
            if goal.progress.no_progress_rounds >= 2 {
                directive.push_str("\nTwo rounds made no material progress. Use a different evidence-backed approach to the unresolved requirement now. Do not repeat unchanged reviews or generate more reports. If no credible route remains, report the exact blocker.\n");
            }
        }
        Some(GoalContinuationPlan {
            directive,
            stop_pattern,
            strategy_rec,
        })
    }

    fn record_and_emit_premature_stop(&self, pattern: &'static str) {
        self.goal_tracker.lock().append_history(
            crate::session::goal_tracker::GoalHistoryEntry::now(
                crate::session::goal_tracker::GoalEvent::PrematureStopDetected,
                Some(pattern.to_owned()),
            ),
        );
        self.emit_event(crate::session::events::Event::GoalPrematureStopDetected { pattern });
    }

    pub(super) fn consume_strategist_note(&self, delivered_rec: &str) {
        if let Some(o) = self.goal_tracker.lock().snapshot_mut()
            && o.last_strategy_recommendation.as_deref() == Some(delivered_rec)
        {
            o.last_strategy_recommendation = None;
            o.last_strategy_path = None;
        }
    }

    pub(super) async fn run_goal_round_end(&self) -> GoalRoundDecision {
        self.evaluate_goal_progress(false).await
    }

    pub(super) async fn run_goal_progress_checkpoint(&self) -> GoalRoundDecision {
        self.evaluate_goal_progress(true).await
    }

    async fn evaluate_goal_progress(&self, checkpoint: bool) -> GoalRoundDecision {
        use crate::session::goal_evaluator::GoalEvaluatorDecision;
        if !laziness_injection_active(
            self.goal_harness_enabled(),
            self.goal_tracker.lock().status(),
        ) {
            return GoalRoundDecision::EndTurn;
        }
        let signals = self.goal_work_signals().await;
        if checkpoint && let Some(decision) = self.skip_goal_checkpoint(&signals).await {
            return decision;
        }
        let verdict = match self.evaluate_goal_round(checkpoint, &signals).await {
            Ok(verdict) => verdict,
            Err(error) => {
                self.log_goal_evaluation(serde_json::json!({
                    "kind": if checkpoint { "checkpoint" } else { "round_end" },
                    "action": "pause_infra",
                    "error": error,
                }))
                .await;
                if !checkpoint {
                    self.record_goal_round_progress(&error, true);
                }
                self.auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Infra,
                    format!(
                        "Goal evaluation failed after a bounded retry: {error}. \
                         The goal was paused rather than treated as complete. \
                         Use /goal resume to retry."
                    ),
                )
                .await;
                return GoalRoundDecision::EndTurn;
            }
        };
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        if self.enforce_goal_token_budget(current_tokens).await {
            return GoalRoundDecision::EndTurn;
        }
        let progress_result = {
            let mut tracker = self.goal_tracker.lock();
            let Some(goal) = tracker.snapshot_mut() else {
                return GoalRoundDecision::EndTurn;
            };
            if goal.status != crate::session::goal_tracker::GoalStatus::Active {
                return GoalRoundDecision::EndTurn;
            }
            let target_changed = verdict
                .verification_target
                .as_ref()
                .is_some_and(|target| Some(target) != goal.progress.verification_target.as_ref());
            let has_gaps = goal
                .last_classifier_gaps
                .as_ref()
                .is_some_and(|gaps| !gaps.is_empty());
            let harness_progress = goal
                .seen_work
                .has_new_work_beyond(&goal.credited_work, &signals);
            let observed = goal.seen_work.harness_observed(&signals);
            let result = goal.progress.record(&verdict, harness_progress).map(|()| {
                (
                    goal.progress.no_progress_rounds,
                    target_changed,
                    has_gaps,
                    observed,
                )
            });
            if result.is_ok() {
                goal.seen_work.observe(&signals);
                let step = verdict.next_step.trim();
                goal.last_evaluator_next_step = (!step.is_empty()).then(|| step.to_owned());
            }
            if result.is_ok() && target_changed {
                // A resumed reviewer would still have the previous worktree as cwd.
                goal.skeptic0_session_id = None;
            }
            self.goal_notify_sender().persist_goal_state(&tracker);
            result
        };
        let (no_progress_rounds, target_changed, has_gaps, observed) = match progress_result {
            Ok(rounds) => rounds,
            Err(error) => {
                self.auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Infra,
                    format!("Could not reconcile goal evidence: {error}"),
                )
                .await;
                return GoalRoundDecision::EndTurn;
            }
        };
        let recheck = verdict.decision == GoalEvaluatorDecision::Recheck
            && has_gaps
            && (no_progress_rounds == 0 || target_changed);
        // A goal never pauses for lack of progress. Once evaluations stop
        // seeing it, the goal is told to change approach.
        let escalate = no_progress_rounds >= GOAL_ESCALATE_AFTER_STALLED_EVALUATIONS
            && verdict.decision != GoalEvaluatorDecision::Blocked
            && (checkpoint
                || (verdict.decision != GoalEvaluatorDecision::CandidateComplete && !recheck));
        let resolved_after = (no_progress_rounds == 0)
            .then(|| self.finish_goal_escalation())
            .flatten();
        let action = if verdict.requires_user() {
            "pause_for_user"
        } else if escalate {
            "escalate"
        } else if checkpoint {
            "continue"
        } else {
            match verdict.decision {
                GoalEvaluatorDecision::Recheck if recheck => "recheck",
                GoalEvaluatorDecision::CandidateComplete => "verify",
                GoalEvaluatorDecision::Blocked => "count_blocker",
                _ => "continue",
            }
        };
        self.log_goal_evaluation(serde_json::json!({
            "kind": if checkpoint { "checkpoint" } else { "round_end" },
            "action": action,
            "evaluator": "main",
            "decision": verdict.decision,
            "next_step": verdict.next_step,
            "blocker_key": verdict.blocker_key,
            "blocker_kind": verdict.blocker_kind,
            "observations": verdict.observations.len(),
            "no_progress_rounds": no_progress_rounds,
            "escalation_resolved_after_runs": resolved_after,
            "workspace_changed": observed.get("workspace_changed_since_last_evaluation"),
            "new_check_outcomes": observed
                .get("new_check_outcomes")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len),
            "delivery_root": signals.delivery_root,
        }))
        .await;
        if escalate {
            self.run_goal_escalation(&verdict.evidence, &verdict.next_step, no_progress_rounds)
                .await;
        }
        // Retrying cannot clear a blocker only the user can: pause at the first
        // evaluation that confirms it, checkpoint or not.
        if verdict.requires_user() {
            if !checkpoint {
                self.record_goal_round_progress(&verdict.evidence, false);
            }
            self.pause_goal_for_user(&verdict).await;
            return GoalRoundDecision::EndTurn;
        }
        if checkpoint {
            // Checkpoints cannot complete a goal or launch a delivery review.
            return GoalRoundDecision::Continue(String::new());
        }
        match verdict.decision {
            GoalEvaluatorDecision::Continue | GoalEvaluatorDecision::Recheck => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    tracker.reset_evaluator_blocker();
                    self.goal_notify_sender().persist_goal_state(&tracker);
                }
                self.record_goal_round_progress(&verdict.evidence, false);
                if recheck {
                    self.verify_goal_candidate(true).await;
                }
            }
            GoalEvaluatorDecision::CandidateComplete => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    tracker.reset_evaluator_blocker();
                    self.goal_notify_sender().persist_goal_state(&tracker);
                }
                self.record_goal_round_progress(&verdict.evidence, false);
                self.verify_goal_candidate(false).await;
            }
            GoalEvaluatorDecision::Blocked => {
                self.record_goal_round_progress(&verdict.evidence, false);
                let streak = {
                    let mut tracker = self.goal_tracker.lock();
                    let streak = tracker.record_evaluator_blocker(verdict.blocker_key.trim());
                    self.goal_notify_sender().persist_goal_state(&tracker);
                    streak
                };
                if streak >= 3
                    && self
                        .auto_pause_goal_if_active_with_message(
                            crate::session::goal_tracker::GoalPauseReason::Verification,
                            format!(
                                "{}\nNext user action: {}",
                                verdict.evidence, verdict.next_step
                            ),
                        )
                        .await
                {
                    self.send_slash_command_output(&format!(
                        "Goal paused — verification blocked.\nReason: {}\n\n{}\n\n{GOAL_DECISION_HINT}",
                        verdict.evidence, verdict.next_step
                    ))
                    .await;
                }
            }
        }
        if self.goal_tracker.lock().status()
            != Some(crate::session::goal_tracker::GoalStatus::Active)
        {
            return GoalRoundDecision::EndTurn;
        }
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let Some(mut plan) = self
            .prepare_goal_continuation(current_tokens, GoalContinuationPurpose::TurnEnd)
            .await
        else {
            return GoalRoundDecision::EndTurn;
        };
        if let Some(rec) = plan.strategy_rec.as_deref() {
            self.consume_strategist_note(rec);
        }
        if let Some(pattern) = plan.stop_pattern {
            self.record_and_emit_premature_stop(pattern);
        }
        GoalRoundDecision::Continue(plan.directive)
    }

    /// A checkpoint that sees new work no evaluation counted yet, with no
    /// blocker pending, makes no model call: the new work is progress, which
    /// is all a checkpoint evaluation would record for it. The work stays
    /// unweighed for the next evaluation, which sees it in `harness_observed`.
    async fn skip_goal_checkpoint(
        &self,
        signals: &crate::session::goal_evaluator::GoalWorkSignals,
    ) -> Option<GoalRoundDecision> {
        let observed = {
            let mut tracker = self.goal_tracker.lock();
            let goal = tracker.snapshot_mut()?;
            if goal.evaluator_blocker_key.is_some()
                || !goal
                    .seen_work
                    .has_new_work_beyond(&goal.credited_work, signals)
            {
                return None;
            }
            let observed = goal.seen_work.harness_observed(signals);
            goal.credited_work.observe(signals);
            goal.progress.no_progress_rounds = 0;
            self.goal_notify_sender().persist_goal_state(&tracker);
            observed
        };
        let resolved_after = self.finish_goal_escalation();
        self.log_goal_evaluation(serde_json::json!({
            "kind": "checkpoint",
            "action": "continue",
            "evaluator": "skipped",
            "no_progress_rounds": 0,
            "escalation_resolved_after_runs": resolved_after,
            "workspace_changed": observed.get("workspace_changed_since_last_evaluation"),
            "new_check_outcomes": observed
                .get("new_check_outcomes")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len),
            "delivery_root": signals.delivery_root,
        }))
        .await;
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        if self.enforce_goal_token_budget(current_tokens).await {
            return Some(GoalRoundDecision::EndTurn);
        }
        Some(GoalRoundDecision::Continue(String::new()))
    }

    /// A stalled goal gets a change-of-approach directive: the main model keeps
    /// the goal and must stop repeating the work that stalled.
    async fn run_goal_escalation(&self, assessment: &str, next_step: &str, stalled: u32) {
        use crate::session::goal_tracker::{GoalEvent, GoalHistoryEntry};
        let runs = {
            let mut tracker = self.goal_tracker.lock();
            let runs = tracker.note_escalation();
            tracker.append_history(GoalHistoryEntry::now(
                GoalEvent::Escalated,
                Some(format!(
                    "escalation {runs} after {stalled} evaluations without progress"
                )),
            ));
            self.goal_notify_sender().persist_goal_state(&tracker);
            runs
        };
        self.log_goal_evaluation(serde_json::json!({
            "kind": "escalation",
            "action": "change_approach",
            "stalled_evaluations": stalled,
            "escalations": runs,
        }))
        .await;
        self.chat_state_handle.push_user_message(ConversationItem::system_reminder(format!(
            "{stalled} goal evaluations in a row found no verifiable progress.\n\nEvaluator assessment: {assessment}\nEvaluator next step: {next_step}\n\nRe-examine the evidence, drop the approach that stalled, and advance a pending criterion with a check whose result is new. If only the user can resolve the blocker, report it."
        )));
    }

    /// Pauses for a blocker only the user can clear, with the evaluator's user
    /// action and options.
    async fn pause_goal_for_user(
        &self,
        verdict: &crate::session::goal_evaluator::GoalEvaluatorVerdict,
    ) {
        {
            let mut tracker = self.goal_tracker.lock();
            tracker.record_evaluator_blocker(verdict.blocker_key.trim());
            self.goal_notify_sender().persist_goal_state(&tracker);
        }
        if self
            .auto_pause_goal_if_active_with_message(
                crate::session::goal_tracker::GoalPauseReason::Verification,
                format!(
                    "{}\nNext user action: {}",
                    verdict.evidence, verdict.next_step
                ),
            )
            .await
        {
            self.send_slash_command_output(&format!(
                "Goal paused — it needs your decision.\nReason: {}\n\n{}\n\n{GOAL_DECISION_HINT}",
                verdict.evidence, verdict.next_step
            ))
            .await;
        }
    }

    /// Progress returned after a stall. Returns how many escalations the stall
    /// took, if any.
    fn finish_goal_escalation(&self) -> Option<u32> {
        use crate::session::goal_tracker::{GoalEvent, GoalHistoryEntry};
        let mut tracker = self.goal_tracker.lock();
        let runs = tracker.finish_escalation()?;
        tracker.append_history(GoalHistoryEntry::now(
            GoalEvent::EscalationResolved,
            Some(format!("progress observed after {runs} escalation(s)")),
        ));
        self.goal_notify_sender().persist_goal_state(&tracker);
        Some(runs)
    }

    /// Appends one record to the goal's `evaluations.jsonl`, the audit trail of
    /// why the goal continued, escalated or paused. Best effort.
    async fn log_goal_evaluation(&self, record: serde_json::Value) {
        use tokio::io::AsyncWriteExt;
        let (path, goal_id) = {
            let tracker = self.goal_tracker.lock();
            let Some(goal) = tracker.snapshot() else {
                return;
            };
            (tracker.evaluations_path(), goal.goal_id.clone())
        };
        let mut record = match record {
            serde_json::Value::Object(fields) => fields,
            _ => return,
        };
        record.insert("ts".into(), chrono::Utc::now().to_rfc3339().into());
        record.insert("goal_id".into(), goal_id.into());
        let mut line = serde_json::Value::Object(record).to_string();
        line.push('\n');
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let written = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            // A tokio file finishes the write in the background unless
            // flushed; the next record or reader must see this one.
            Ok(mut file) => match file.write_all(line.as_bytes()).await {
                Ok(()) => file.flush().await,
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            tracing::debug!(%error, path = %path.display(), "goal evaluation log write failed");
        }
    }

    pub(super) async fn inject_goal_continuation_message(&self, directive: String) {
        self.prune_prior_goal_continuation_directives().await;
        self.chat_state_handle
            .push_user_message(ConversationItem::goal_summary(directive));
    }

    pub(super) async fn maybe_queue_goal_continuation(&self) {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let Some(plan) = self
            .prepare_goal_continuation(current_tokens, GoalContinuationPurpose::TurnEnd)
            .await
        else {
            return;
        };
        {
            let state = self.state.lock().await;
            if state.pending_inputs.iter().any(|i| {
                matches!(
                    i.input_origin.as_prompt_origin(),
                    super::super::PromptOrigin::GoalSummary
                        | super::super::PromptOrigin::GoalClassifierNudge
                )
            }) {
                return;
            }
        }
        self.prune_prior_goal_continuation_directives().await;

        let prompt_id = format!("goal-summary-{}", uuid::Uuid::now_v7());
        let (respond_to, _) = tokio::sync::oneshot::channel();
        {
            let mut state = self.state.lock().await;
            if state.pending_inputs.iter().any(|i| {
                matches!(
                    i.input_origin.as_prompt_origin(),
                    super::super::PromptOrigin::GoalSummary
                        | super::super::PromptOrigin::GoalClassifierNudge
                )
            }) {
                tracing::debug!("continuation reminder already pending; skipping duplicate");
                return;
            }
            state.pending_inputs.push_back(InputItem {
                prompt_id,
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    plan.directive,
                ))],
                prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                trace_gcs_config: None,
                artifact_tracker: None,
                client_identifier: None,
                screen_mode: None,
                verbatim: true,
                json_schema: None,
                input_origin: InputOrigin::new(super::super::PromptOrigin::GoalSummary),
                task_wake_fallback: None,
                tool_overrides_update: None,
                respond_to,
                persist_ack: None,
                parsed_prompt_tx: None,
                initial_child_prompt_ready: None,
                queue_meta: None,
                queue_mutation_policy: QueueMutationPolicy::hidden(),
                send_now: false,
                traceparent: None,
            });
        }
        if let Some(rec) = plan.strategy_rec.as_deref() {
            self.consume_strategist_note(rec);
        }
        if let Some(pattern) = plan.stop_pattern {
            self.record_and_emit_premature_stop(pattern);
        }
    }

    pub(crate) async fn auto_pause_goal_if_active(
        &self,
        reason: crate::session::goal_tracker::GoalPauseReason,
    ) {
        let _ = self
            .auto_pause_goal_if_active_inner(reason, None, None)
            .await;
    }

    pub(crate) async fn auto_pause_goal_if_active_with_message(
        &self,
        reason: crate::session::goal_tracker::GoalPauseReason,
        message: String,
    ) -> bool {
        self.auto_pause_goal_if_active_inner(reason, Some(message), None)
            .await
    }

    /// Pause only if the active goal still has `goal_id`.
    /// Stale planner work uses this variant, so a replacement goal created while the planner ran cannot be paused by the previous goal's failure.
    pub(crate) async fn auto_pause_goal_if_matches_with_message(
        &self,
        goal_id: &str,
        reason: crate::session::goal_tracker::GoalPauseReason,
        message: String,
    ) -> bool {
        self.auto_pause_goal_if_active_inner(reason, Some(message), Some(goal_id))
            .await
    }

    /// Shared auto-pause body: pauses the goal (with `message`, else the bare reason) and emits, but only when the goal is `Active`.
    /// When `expected_goal_id` is `Some`, the goal must also still carry that id.
    async fn auto_pause_goal_if_active_inner(
        &self,
        reason: crate::session::goal_tracker::GoalPauseReason,
        message: Option<String>,
        expected_goal_id: Option<&str>,
    ) -> bool {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        {
            let mut tracker = self.goal_tracker.lock();
            let is_match = match expected_goal_id {
                Some(goal_id) => tracker.snapshot().is_some_and(|goal| {
                    goal.goal_id == goal_id
                        && goal.status == crate::session::goal_tracker::GoalStatus::Active
                }),
                None => tracker.status() == Some(crate::session::goal_tracker::GoalStatus::Active),
            };
            if !is_match {
                return false;
            }
            match message {
                Some(msg) => tracker.pause_with_message(reason, msg),
                None => tracker.pause(reason),
            };
        }
        self.clear_pending_classifier_completions();
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let notify = self.goal_notify_sender();
        notify.emit_goal_updated(
            &mut self.goal_tracker.lock(),
            tokens_used,
            finished_marginal,
        );
        self.emit_event(crate::session::events::Event::GoalAutoPaused {
            reason: reason.into(),
        });
        true
    }

    async fn assistant_text_signals_premature_stop(&self) -> Option<&'static str> {
        let text = self.chat_state_handle.get_last_assistant_text().await?;
        crate::session::goal_stop_detector::matched_stop_pattern(&text)
    }

    async fn has_pending_goal_todos(&self) -> bool {
        use crate::tools::todo::{TodoState, TodoStatus};
        use distill_tools::types::resources::State;
        let bridge = self.tool_bridge_handle();
        match bridge.read_resource::<State<TodoState>>().await {
            Some(state) => state.0.todo_items_with_ids().any(|(_id, item)| {
                matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress)
            }),
            None => {
                tracing::warn!(
                    "goal stop-detector: TodoState resource missing; failing OPEN \
                     so the bail-text safety net still queues the continuation",
                );
                true
            }
        }
    }
}

impl SessionActor {
    pub(super) async fn drain_goal_updates(&self, current_tokens: i64, purpose: DrainPurpose) {
        self.drain_goal_updates_with_extra(current_tokens, purpose, Vec::new())
            .await;
    }

    pub(super) async fn drain_goal_updates_with_extra(
        &self,
        current_tokens: i64,
        purpose: DrainPurpose,
        extra: Vec<distill_tools::implementations::distill::update_goal::UpdateGoalEnvelope>,
    ) {
        use distill_tools::implementations::distill::update_goal::{RejectReason, UpdateGoalAck};
        if !self.goal_harness_enabled() {
            let reject = || UpdateGoalAck::Rejected {
                reason: RejectReason::HarnessDisabled,
                detail: "Goal mode is not active for this session (no /goal run in \
                         progress); update_goal has no effect."
                    .to_string(),
            };
            for (_input, ack_tx) in extra {
                send_ack(ack_tx, reject());
            }
            if let Some(rx) = self.goal_update_rx.borrow_mut().as_mut() {
                while let Ok((_input, ack_tx)) = rx.try_recv() {
                    send_ack(ack_tx, reject());
                }
            }
            return;
        }
        let mut cmds: Vec<DrainSource> = match purpose {
            DrainPurpose::TurnEnd => self
                .pending_classifier_completions
                .lock()
                .drain(..)
                .map(DrainSource::Pending)
                .collect(),
            DrainPurpose::MidTurn => Vec::new(),
        };
        cmds.extend(extra.into_iter().map(DrainSource::Channel));
        {
            let mut rx_slot = self.goal_update_rx.borrow_mut();
            if let Some(rx) = rx_slot.as_mut() {
                cmds.extend(std::iter::from_fn(|| rx.try_recv().ok()).map(DrainSource::Channel));
            }
        }
        let notify = self.goal_notify_sender();
        let mut block_seen = false;
        let mut cap_dropped: u32 = 0;
        let mut auto_paused_mid_drain = false;
        for source in cmds {
            let (cmd, ack_tx) = source.into_parts();
            if auto_paused_mid_drain {
                cap_dropped = cap_dropped.saturating_add(1);
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::DroppedAfterPauseInDrain,
                        detail: "Goal auto-paused during verification; this completion was \
                                 dropped without further verification"
                            .to_string(),
                    },
                );
                drop(cmd);
                continue;
            }
            if block_seen {
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::BlockSeenInDrain,
                        detail: "Goal just transitioned to Blocked by a prior update in this drain"
                            .to_string(),
                    },
                );
                continue;
            }
            if let Some(reason) = cmd.blocked_reason {
                let streak = self
                    .goal_blocked_streak
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                if streak < 3 {
                    tracing::info!(
                        streak,
                        "blocked attempt rejected — model must try {}/3 before blocking",
                        streak,
                    );
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Accepted {
                            summary: format!(
                                "Blocked attempt {streak}/3 recorded. The harness needs \
                                 3 consecutive blocked attempts before pausing the goal — \
                                 continue retrying or refining the approach."
                            ),
                        },
                    );
                    continue;
                }
                let detail = cmd.message;
                let chat_text = format_blocked_chat_notification(&reason, detail.as_deref());
                let success_summary = format!("Goal blocked: {reason}.");
                let pause_msg = match detail {
                    Some(d) => format!("{reason}\n{d}"),
                    None => reason,
                };
                let applied = self
                    .auto_pause_goal_if_active_with_message(
                        crate::session::goal_tracker::GoalPauseReason::Verification,
                        pause_msg,
                    )
                    .await;
                if applied {
                    self.send_slash_command_output(&chat_text).await;
                    block_seen = true;
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Accepted {
                            summary: success_summary,
                        },
                    );
                } else {
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::BlockedAgainstNonActive,
                            detail: "Goal is not Active; blocked_reason ignored".to_string(),
                        },
                    );
                }
                continue;
            }
            if cmd.completed != Some(true) {
                let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                let mut tracker = self.goal_tracker.lock();
                notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                drop(tracker);
                let summary = cmd
                    .message
                    .clone()
                    .map(|m| format!("{m}."))
                    .unwrap_or_else(|| "Goal updated.".to_string());
                try_send_ack(ack_tx, UpdateGoalAck::Accepted { summary });
                continue;
            }

            self.goal_blocked_streak
                .store(0, std::sync::atomic::Ordering::Relaxed);

            let policy = self.resolve_goal_classifier_policy();

            if !policy.enabled {
                let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                self.prune_subagent_records_for_active_goal();
                self.clear_pending_classifier_completions();
                let mut tracker = self.goal_tracker.lock();
                tracker.complete();
                notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                drop(tracker);
                try_send_ack(ack_tx, UpdateGoalAck::CompletedWithoutClassifier);
                continue;
            }

            let non_active_meta: Option<(u32, bool)> = {
                let tracker = self.goal_tracker.lock();
                match tracker.snapshot() {
                    Some(o) if o.status == crate::session::goal_tracker::GoalStatus::Active => None,
                    Some(o) => Some((
                        o.classifier_runs_attempted,
                        o.status == crate::session::goal_tracker::GoalStatus::BackOffPaused
                            && o.classifier_max_runs
                                .is_some_and(|cap| o.classifier_runs_attempted >= cap),
                    )),
                    None => Some((0, false)),
                }
            };
            if let Some((attempts_seen, post_cap)) = non_active_meta {
                tracing::debug!(
                    "completed: true against non-Active goal; dropping classifier fire"
                );
                if post_cap {
                    self.events.emit(
                        crate::session::events::Event::GoalClassifierDroppedAfterCap {
                            attempts_seen,
                        },
                    );
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::PostCap,
                            detail: format!(
                                "Classifier cap already reached ({attempts_seen} attempts); goal \
                                 is BackOff-paused. Do not call update_goal again until the user \
                                 resumes."
                            ),
                        },
                    );
                } else {
                    self.events.emit(
                        crate::session::events::Event::GoalClassifierFailOpen {
                            reason: crate::session::events::GoalClassifierFailOpenReason::GoalNotActiveAtResolve
                                .as_const_str(),
                            attempt: attempts_seen,
                            latency_ms: 0,
                        },
                    );
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::NonActive,
                            detail: "Goal is not Active; cannot mark complete".to_string(),
                        },
                    );
                }
                continue;
            }

            if matches!(purpose, DrainPurpose::MidTurn) {
                let pending_depth = {
                    let mut q = self.pending_classifier_completions.lock();
                    if q.len() >= GOAL_CLASSIFIER_PENDING_QUEUE_CAP {
                        q.pop_front();
                        self.events.emit(
                            crate::session::events::Event::GoalClassifierFailClosed {
                                reason:
                                    crate::session::events::GoalClassifierFailClosedReason::PendingQueueFull
                                        .as_const_str(),
                                attempt: 0,
                            },
                        );
                    }
                    q.push_back(cmd);
                    u32::try_from(q.len()).unwrap_or(u32::MAX)
                };
                self.events.emit(
                    crate::session::events::Event::GoalClassifierMidTurnDeferred { pending_depth },
                );
                try_send_ack(ack_tx, UpdateGoalAck::DeferredToTurnEnd { pending_depth });
                continue;
            }

            if self
                .goal_classifier_in_flight
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_err()
            {
                self.events
                    .emit(crate::session::events::Event::GoalClassifierFailClosed {
                    reason:
                        crate::session::events::GoalClassifierFailClosedReason::ConcurrentInFlight
                            .as_const_str(),
                    attempt: 0,
                });
                let synthetic_info = self
                    .account_not_achieved_without_sampler(
                        &policy,
                        NotAchievedSyntheticReason::ConcurrentInFlight,
                    )
                    .await;
                if let Some((attempt, details_path, cap_reached)) = synthetic_info {
                    if cap_reached {
                        if self.cap_just_reached() {
                            auto_paused_mid_drain = true;
                        }
                        try_send_ack(
                            ack_tx,
                            UpdateGoalAck::ClassifierCapReached {
                                details_path,
                                attempt,
                            },
                        );
                    } else {
                        try_send_ack(
                            ack_tx,
                            UpdateGoalAck::ClassifierConcurrentInFlight {
                                details_path,
                                attempt,
                                max_runs: policy.max_runs,
                            },
                        );
                    }
                } else {
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::InFlightOrchestrationVanished,
                            detail: "Goal orchestration vanished during in-flight short-circuit"
                                .to_string(),
                        },
                    );
                }
                continue;
            }
            let _in_flight_guard = InFlightGuard {
                flag: &self.goal_classifier_in_flight,
            };
            let fire_started = std::time::Instant::now();

            let Some(attempt) = self.reserve_classifier_attempt_slot(&policy) else {
                self.events
                    .emit(crate::session::events::Event::GoalClassifierFailOpen {
                    reason:
                        crate::session::events::GoalClassifierFailOpenReason::GoalNotActiveAtResolve
                            .as_const_str(),
                    attempt: 0,
                    latency_ms: fire_started.elapsed().as_millis() as u64,
                });
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::OrchestrationVanished,
                        detail: "Goal orchestration vanished before classifier could start"
                            .to_string(),
                    },
                );
                continue;
            };

            self.emit_goal_verifying(current_tokens);

            let mut slot_guard = TrackerDropGuard::new(&self.goal_tracker, |t| {
                use crate::session::goal_tracker::GoalStatus;
                if !matches!(
                    t.status(),
                    Some(
                        GoalStatus::BackOffPaused
                            | GoalStatus::NoProgressPaused
                            | GoalStatus::Blocked
                    ),
                ) {
                    t.rollback_classifier_attempt();
                }
            });

            let outcome = {
                let _verifying_latch = TrackerDropGuard::new(&self.goal_tracker, |t| {
                    if let Some(o) = t.snapshot_mut() {
                        o.verifying_in_flight = false;
                    }
                });
                self.run_verification_stage_for_drain(attempt, policy.max_runs, false)
                    .await
            };

            let status_changed = self.goal_tracker.lock().status()
                != Some(crate::session::goal_tracker::GoalStatus::Active);
            if status_changed {
                self.events
                    .emit(crate::session::events::Event::GoalClassifierFailOpen {
                    reason:
                        crate::session::events::GoalClassifierFailOpenReason::GoalNotActiveAtResolve
                            .as_const_str(),
                    attempt,
                    latency_ms: fire_started.elapsed().as_millis() as u64,
                });
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::StatusChangedDuringClassifier,
                        detail: "Goal transitioned to non-Active during classifier run".to_string(),
                    },
                );
                continue;
            }
            slot_guard.disarm();

            let ack = self
                .apply_classifier_outcome_legacy(&policy, attempt, outcome, &notify)
                .await;
            if !auto_paused_mid_drain && self.goal_is_paused() {
                auto_paused_mid_drain = true;
            }
            try_send_ack(ack_tx, ack);
        }
        if cap_dropped > 0 {
            self.events.emit(
                crate::session::events::Event::GoalClassifierPendingQueueCleared {
                    dropped: cap_dropped,
                },
            );
        }
    }

    fn cap_just_reached(&self) -> bool {
        let tracker = self.goal_tracker.lock();
        tracker.snapshot().is_some_and(|o| {
            o.status == crate::session::goal_tracker::GoalStatus::BackOffPaused
                && o.classifier_max_runs
                    .is_some_and(|cap| o.classifier_runs_attempted >= cap)
        })
    }

    fn goal_is_paused(&self) -> bool {
        self.goal_tracker
            .lock()
            .status()
            .is_some_and(|s| s.is_paused())
    }

    pub(crate) fn clear_pending_classifier_completions(&self) {
        self.pending_classifier_completions.lock().clear();
    }

    /// In-turn goal loop step: run verification for the round just completed.
    /// Decides whether to continue the loop in-turn (with the continuation directive) or end the turn.
    /// The premature-stop signal, if any, is emitted here, once per continued round.
    pub(super) async fn run_goal_round_end_legacy(&self) -> GoalRoundDecision {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let Some(plan) = self
            .prepare_goal_continuation(current_tokens, GoalContinuationPurpose::TurnEnd)
            .await
        else {
            return GoalRoundDecision::EndTurn;
        };
        // A Continue directive is unconditionally injected; returning it commits the embedded strategist note for delivery
        if let Some(rec) = plan.strategy_rec.as_deref() {
            self.consume_strategist_note(rec);
        }
        if let Some(pattern) = plan.stop_pattern {
            self.record_and_emit_premature_stop(pattern);
        }
        GoalRoundDecision::Continue(plan.directive)
    }

    async fn account_not_achieved_without_sampler(
        &self,
        policy: &GoalClassifierPolicy,
        reason: NotAchievedSyntheticReason,
    ) -> Option<(u32, String, bool)> {
        use crate::session::goal_classifier::format_details_path;
        use crate::session::goal_tracker::GoalClassifierVerdict;

        let (attempt, verifier_id) = {
            let mut tracker = self.goal_tracker.lock();
            let o = tracker.snapshot_mut()?;
            o.classifier_runs_attempted = o.classifier_runs_attempted.saturating_add(1);
            o.classifier_max_runs = Some(policy.max_runs);
            (o.classifier_runs_attempted, o.verifier_id.clone())
        };
        let details_path = format_details_path(&verifier_id, attempt);
        let body = reason.details_body(attempt, policy.max_runs);
        let wrote_details =
            if crate::session::goal_tracker::ensure_goal_scratch_root(&verifier_id).is_ok() {
                tokio::fs::write(&details_path, body).await.is_ok()
            } else {
                false
            };
        let details_ptr: Option<&str> = wrote_details.then_some(details_path.as_str());

        let gaps_summary = reason.gaps_summary();
        {
            let mut tracker = self.goal_tracker.lock();
            Self::record_verdict_on_orchestration(
                &mut tracker,
                GoalClassifierVerdict::NotAchieved,
                details_ptr,
                GapsUpdate::Preserve,
            );
            tracker.record_not_achieved_streak();
        }

        let cap_reached = attempt >= policy.max_runs;
        if cap_reached {
            let pause_summary = format!(
                "{}:\n{gaps_summary}",
                crate::session::goal_classifier::PAUSE_GROUP_FIXABLE,
            );
            self.auto_pause_for_classifier_cap(attempt, details_ptr.unwrap_or(""), &pause_summary)
                .await;
        } else {
            let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
            let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }
        Some((attempt, details_ptr.unwrap_or("").to_owned(), cap_reached))
    }

    async fn apply_classifier_outcome_legacy(
        &self,
        policy: &GoalClassifierPolicy,
        attempt: u32,
        outcome: crate::session::goal_classifier::GoalClassifierOutcome,
        notify: &crate::session::goal_orchestrator::GoalNotifySender,
    ) -> distill_tools::implementations::distill::update_goal::UpdateGoalAck {
        use crate::session::goal_classifier::GoalClassifierOutcome;
        use crate::session::goal_tracker::GoalClassifierVerdict;
        use distill_tools::implementations::distill::update_goal::UpdateGoalAck;

        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        match outcome {
            GoalClassifierOutcome::Achieved { details_path } => {
                self.prune_subagent_records_for_active_goal();
                self.clear_pending_classifier_completions();
                let details_path = {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::Achieved,
                        Some(details_path.as_str()),
                        GapsUpdate::Clear,
                    );
                    tracker.reset_strategist_state();
                    tracker.complete();
                    let details_path = tracker
                        .snapshot()
                        .and_then(|o| o.last_classifier_details_path.clone())
                        .unwrap_or(details_path);
                    notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                    details_path
                };
                self.maybe_run_goal_summarizer(attempt).await;
                UpdateGoalAck::ClassifierAchieved { details_path }
            }
            GoalClassifierOutcome::NotAchieved {
                details_path,
                gaps_summary,
                pause_summary,
                gap_fingerprint,
            } => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::NotAchieved,
                        Some(details_path.as_str()),
                        GapsUpdate::Set(gaps_summary.as_str()),
                    );
                    tracker.record_not_achieved_streak();
                }
                if attempt >= policy.max_runs {
                    self.auto_pause_for_classifier_cap(attempt, &details_path, &pause_summary)
                        .await;
                    return UpdateGoalAck::ClassifierCapReached {
                        details_path,
                        attempt,
                    };
                }
                let stalled = !gap_fingerprint.is_empty()
                    && self
                        .goal_tracker
                        .lock()
                        .record_classifier_stall(&gap_fingerprint);
                if stalled {
                    self.escalate_classifier_stall(attempt, &gaps_summary).await;
                } else {
                    let every = self.goal_strategist_every;
                    let claimed = self.goal_tracker.lock().claim_strategist_fire(
                        |consecutive, last_fired| {
                            crate::session::goal_strategist::strategist_should_fire(
                                consecutive,
                                last_fired,
                                every,
                            )
                        },
                    );
                    if let Some(consecutive_not_achieved) = claimed {
                        self.maybe_run_goal_strategist(attempt, consecutive_not_achieved)
                            .await;
                    }
                }
                notify.emit_goal_updated(
                    &mut self.goal_tracker.lock(),
                    tokens_used,
                    finished_marginal,
                );
                UpdateGoalAck::ClassifierNotAchieved {
                    details_path,
                    attempt,
                    max_runs: policy.max_runs,
                }
            }
            GoalClassifierOutcome::Blocked {
                details_path,
                pause_summary,
            } => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::NotAchieved,
                        Some(details_path.as_str()),
                        GapsUpdate::Clear,
                    );
                    tracker.rollback_classifier_attempt();
                    tracker.reset_classifier_stall();
                    tracker.reset_strategist_state();
                }
                self.auto_pause_for_classifier_blocked(&details_path, &pause_summary)
                    .await;
                UpdateGoalAck::ClassifierBlocked { details_path }
            }
            GoalClassifierOutcome::FailOpenAchieved {
                reason,
                details_path,
            } => {
                tracing::warn!(
                    ?reason,
                    "goal verification fail-open (infra-class) → Achieved",
                );
                self.prune_subagent_records_for_active_goal();
                self.clear_pending_classifier_completions();
                let mut tracker = self.goal_tracker.lock();
                Self::record_verdict_on_orchestration(
                    &mut tracker,
                    GoalClassifierVerdict::Achieved,
                    (!details_path.is_empty()).then_some(details_path.as_str()),
                    GapsUpdate::Clear,
                );
                tracker.reset_strategist_state();
                tracker.complete();
                notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                UpdateGoalAck::ClassifierFailOpenAchieved {
                    reason: reason.as_const_str(),
                }
            }
        }
    }
}

#[cfg(test)]
mod role_capability_tests {
    use super::RoleCapability;
    use distill_tools::implementations::distill::task::types::SubagentTypeSummary;

    fn summary(can_read: bool, can_search: bool, can_execute: bool) -> SubagentTypeSummary {
        SubagentTypeSummary {
            can_read,
            can_search,
            can_execute,
            ..Default::default()
        }
    }

    #[test]
    fn skeptic_requires_read_and_search() {
        assert!(RoleCapability::Skeptic.is_satisfied(&summary(true, true, false)));
        assert!(!RoleCapability::Skeptic.is_satisfied(&summary(true, false, false)));
        assert!(!RoleCapability::Skeptic.is_satisfied(&summary(false, true, false)));
    }

    #[test]
    fn strategist_requires_read_search_execute() {
        assert!(RoleCapability::Strategist.is_satisfied(&summary(true, true, true)));
        assert!(!RoleCapability::Strategist.is_satisfied(&summary(true, true, false)));
        assert!(!RoleCapability::Strategist.is_satisfied(&summary(true, false, true)));
    }
}

#[cfg(test)]
mod role_tool_names_tests {
    use super::{PanelResolveCache, role_tool_names_from};
    use crate::session::goal_planner::RoleSpawnOverride;
    use crate::session::goal_role_tools::RoleToolNames;
    use distill_tools::implementations::distill::task::types::{
        SubagentDescribeOutcome, SubagentTypeSummary,
    };
    use distill_tools::types::tool::ToolKind;

    /// A named summary (distinct from the inherit/default names) so tests can tell `from_summary` output from the inherit fallback.
    fn cursor_summary() -> SubagentTypeSummary {
        let mut s = SubagentTypeSummary {
            can_read: true,
            can_search: true,
            ..Default::default()
        };
        s.tool_names.insert(ToolKind::Read, "cursor_read".into());
        s.tool_names.insert(ToolKind::Search, "cursor_grep".into());
        s
    }

    /// Parent inherit names distinct from the literal defaults, so assertions can tell which path won.
    fn inherit_names() -> RoleToolNames {
        RoleToolNames::from_parent(
            Some("parent_read".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn committed_explicit_draws_from_the_cached_describe_summary() {
        let mut cache = PanelResolveCache::default();
        cache.describe.insert(
            "cursor-type".into(),
            SubagentDescribeOutcome::Ok(cursor_summary()),
        );
        let ov = RoleSpawnOverride {
            model: Some("m".into()),
            agent_type: Some("cursor-type".into()),
        };
        let tn = role_tool_names_from(&ov, &cache, &inherit_names());
        assert_eq!(tn.read, "cursor_read", "explicit pair uses its own toolset");
        assert_eq!(tn.search, "cursor_grep");
        assert!(
            tn.toolset_tools.contains("cursor_read"),
            "explicit path enumerates {{TOOLSET_TOOLS}}",
        );
    }

    #[test]
    fn inherit_override_uses_parent_names() {
        let tn = role_tool_names_from(
            &RoleSpawnOverride::default(),
            &PanelResolveCache::default(),
            &inherit_names(),
        );
        assert_eq!(tn.read, "parent_read", "inherit uses the parent toolset");
        assert_eq!(tn.toolset_tools, "", "inherit carries no toolset block");
    }

    #[test]
    fn explicit_but_summary_absent_or_unavailable_falls_back_to_inherit() {
        // (a) agent_type set but no cache entry (never described).
        let ov = RoleSpawnOverride {
            model: Some("m".into()),
            agent_type: Some("missing".into()),
        };
        let tn = role_tool_names_from(&ov, &PanelResolveCache::default(), &inherit_names());
        assert_eq!(tn.read, "parent_read", "absent summary ⇒ inherit");

        // (b) agent_type set but the describe round-trip failed open (`Unavailable`): inherit wins, never a partial/broken summary
        let mut cache = PanelResolveCache::default();
        cache
            .describe
            .insert("x".into(), SubagentDescribeOutcome::Unavailable);
        let ov = RoleSpawnOverride {
            model: Some("m".into()),
            agent_type: Some("x".into()),
        };
        let tn = role_tool_names_from(&ov, &cache, &inherit_names());
        assert_eq!(tn.read, "parent_read", "fail-open describe ⇒ inherit");
    }
}
