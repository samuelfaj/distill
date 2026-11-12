// Modified for Distill by Samuel Fajreldines, 2026.
//! The per-mode decision policy at the Jev call points: how the effective
//! mode's policy row (`ModePolicy`) shapes the effort decision, which
//! turn-start question the first request waits for, how long the after-edit
//! review (C4) may take and whether the utility model selects tool results.
//! Each enforcement records a content-free `policy:<mode>` row
//! (`crate::jev::record_policy`).

use std::sync::Arc;

use distill_sampling_types::ReasoningEffort;
use distill_tool_types::{EffortCadence, EffortPolicy, ExecutionMode, ModePolicy};
use distill_workspace::jev::catalog::routing;
use distill_workspace::jev::flags::JevLever;

use super::jev_ledger::{EFFORT_SWITCH_FREE_PROMPT_TOKENS, TurnRoute};
use super::jev_routing::{effort_keys_cached_prefix, effort_level_value};
use super::*;

/// Tools whose only job is to wait on or poll background work: the blocking
/// waits `is_interruptible_wait_tool` names and the poll tools the session
/// analyzer counts (`tools/task_cost_eval/session_usage.py`).
const BACKGROUND_WAIT_TOOLS: &[&str] = &[
    "get_task_output",
    "get_command_or_subagent_output",
    "get_task_or_subagent_output",
    "get_terminal_command_output",
    "wait_tasks",
    "wait_commands_or_subagents",
    "wait_tasks_or_subagents",
    "Await",
    "AwaitShell",
    "monitor",
];

#[cfg(test)]
thread_local! {
    static EFFORT_PER_ROUND_FOR_TESTS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Overrides `[modes.ultrafast] effort_per_round` on this thread; `None` reads the config.
#[cfg(test)]
pub(crate) fn set_ultrafast_effort_per_round_for_tests(value: Option<bool>) {
    EFFORT_PER_ROUND_FOR_TESTS.with(|cell| cell.set(value));
}

/// `[modes.ultrafast] effort_per_round`, read once per process (config does
/// not change mid-run); a config that fails to load keeps it off.
fn ultrafast_effort_per_round() -> bool {
    #[cfg(test)]
    if let Some(value) = EFFORT_PER_ROUND_FOR_TESTS.with(std::cell::Cell::get) {
        return value;
    }
    static SETTING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SETTING.get_or_init(|| {
        crate::agent::config::ultrafast_effort_per_round(
            crate::config::load_effective_config().ok().as_ref(),
        )
    })
}

/// The turn's one effort decision under a once-per-turn cadence.
#[derive(Debug, Default)]
pub(crate) enum TurnEffort {
    #[default]
    NotAsked,
    /// Asked in `turn` on a detached task about the `offered` levels.
    Pending {
        turn: u64,
        offered: Vec<routing::EffortChoice>,
        answer: tokio::task::JoinHandle<Option<distill_workspace::jev::JevAnswerSet>>,
    },
    /// The decision of `turn` is taken: applied, failed, or dropped.
    Taken { turn: u64 },
}

impl TurnEffort {
    /// Drops the decision; a question still in flight is cancelled.
    pub(crate) fn abort(self) {
        if let Self::Pending { answer, .. } = self {
            answer.abort();
        }
    }
}

/// The answer a finished detached question gave; `None` for no answer.
fn finished_answer(
    answer: tokio::task::JoinHandle<Option<distill_workspace::jev::JevAnswerSet>>,
) -> Option<distill_workspace::jev::JevAnswerSet> {
    futures::FutureExt::now_or_never(answer)
        .and_then(Result::ok)
        .flatten()
}

/// Whether the round about to be prepared only follows waits on background
/// work, or answers the final-check reminder.
fn idle_round(conversation: &[ConversationItem]) -> bool {
    let mut answers_final_check = false;
    for item in conversation.iter().rev() {
        match item {
            ConversationItem::Assistant(assistant) => {
                let only_waits = !assistant.tool_calls.is_empty()
                    && assistant
                        .tool_calls
                        .iter()
                        .all(|call| BACKGROUND_WAIT_TOOLS.contains(&call.name.as_str()));
                return answers_final_check || only_waits;
            }
            ConversationItem::User(_) => {
                answers_final_check |= item
                    .text_content()
                    .contains(super::turn::JEV_FINAL_CHECK_REMINDER);
            }
            _ => {}
        }
    }
    false
}

impl SessionActor {
    /// The effort rules this round follows: the effective mode's row, or, on
    /// a v2 row, Normal's once UltraFast's ratchet handed the turn to Normal's
    /// effort policy (a today row reads only its fallback and lean from that,
    /// as before). `[modes.ultrafast] effort_per_round` puts UltraFast's v2
    /// row back on today's per-round cadence.
    pub(super) fn round_effort_policy(&self) -> EffortPolicy {
        let modes = super::mode_policy::v2_modes();
        let mut mode = self.execution_mode.load();
        if modes.contains(mode) {
            mode = self.escalation_effort_mode();
        }
        let mut effort = modes.policy(mode).runtime.effort;
        if mode == ExecutionMode::Ultrafast
            && effort.cadence == EffortCadence::PerTurn
            && ultrafast_effort_per_round()
        {
            effort.cadence = ModePolicy::today(ExecutionMode::Ultrafast)
                .runtime
                .effort
                .cadence;
        }
        effort
    }

    /// The level this round's effort may not exceed; `None` without a ceiling
    /// or with an explicit effort.
    pub(super) fn mode_effort_ceiling(&self) -> Option<ReasoningEffort> {
        if !self
            .jev_effort_auto
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        self.round_effort_policy().ceiling.map(effort_level_value)
    }

    /// The level a round keeps when its only tool calls waited on background
    /// work or it answers the final-check reminder: the one this turn last
    /// ran `model` at. `None` when the row asks as usual, the round is not
    /// such a round, or no earlier round of the turn ran on `model`.
    pub(super) async fn idle_round_level(&self, model: &str) -> Option<Option<ReasoningEffort>> {
        if !self.round_effort_policy().idle_rounds_keep_level {
            return None;
        }
        let (last_model, last_effort) = self.jev_ledger.borrow().last_execution.clone()?;
        if last_model != model {
            return None;
        }
        let conversation = self.chat_state_handle.get_conversation().await;
        idle_round(&conversation).then_some(last_effort)
    }

    /// Settles this round's effort without asking when the row says so: the
    /// last level on an idle round (see [`Self::idle_round_level`]), or the
    /// turn's one decision under a once-per-turn cadence. `true` when `cfg`
    /// is settled.
    pub(super) async fn mode_effort_without_asking(&self, cfg: &mut SamplingConfig) -> bool {
        if self.round_effort_policy().cadence == EffortCadence::PerTurn {
            // A kept route already carries the turn's decision.
            if !self.jev_ledger.borrow().round_route_kept {
                self.choose_turn_effort(cfg).await;
            }
            return true;
        }
        let Some(level) = self.idle_round_level(&cfg.model).await else {
            return false;
        };
        cfg.reasoning_effort = level;
        crate::jev::record_policy(self.execution_mode.load(), "effort_kept");
        true
    }

    /// The turn's one effort decision, about the request. It is asked on a
    /// detached task so no request waits for it; until its answer is in, a
    /// round runs at the row's silent level, and a round that keeps the
    /// turn's route takes the answer later (see
    /// [`Self::apply_late_turn_effort`]).
    async fn choose_turn_effort(&self, cfg: &mut SamplingConfig) {
        let turn = self.current_turn_number.get();
        let state = std::mem::take(&mut self.jev_ledger.borrow_mut().turn_effort);
        match state {
            TurnEffort::Pending {
                turn: asked,
                offered,
                answer,
            } if asked == turn => {
                if !answer.is_finished() {
                    self.jev_ledger.borrow_mut().turn_effort = TurnEffort::Pending {
                        turn,
                        offered,
                        answer,
                    };
                    self.apply_effort_fallback(cfg, "the turn's effort answer is not in yet");
                    return;
                }
                self.jev_ledger.borrow_mut().turn_effort = TurnEffort::Taken { turn };
                match finished_answer(answer) {
                    Some(answers) => self.apply_auto_route_effort(
                        cfg,
                        &answers,
                        &offered,
                        routing::MICRO_EFFORT_QUESTION,
                    ),
                    None => self.apply_effort_fallback(cfg, "Jev gave no effort answer"),
                }
            }
            TurnEffort::Taken { turn: taken } if taken == turn => {
                self.jev_ledger.borrow_mut().turn_effort = TurnEffort::Taken { turn };
                // A new route in the same turn (after a compaction or a model
                // switch) keeps the level the turn's answer picked.
                let answered = self
                    .jev_ledger
                    .borrow()
                    .last_answered_effort
                    .clone()
                    .map(|(_, value)| value)
                    .filter(|value| {
                        self.models_manager
                            .model_supports_reasoning_effort_value(&cfg.model, *value)
                    });
                match answered {
                    Some(value) => cfg.reasoning_effort = Some(value),
                    None => self.apply_effort_fallback(cfg, "the turn's effort answer is not in"),
                }
            }
            stale => {
                stale.abort();
                let Some((offered, state, questions)) = self.jev_effort_question(cfg).await else {
                    self.jev_ledger.borrow_mut().turn_effort = TurnEffort::Taken { turn };
                    return;
                };
                let answer = tokio::task::spawn_local(crate::jev::in_current_turn(
                    crate::jev::ask_item(JevLever::B2MicroEffort, state, questions),
                ));
                self.jev_ledger.borrow_mut().turn_effort = TurnEffort::Pending {
                    turn,
                    offered,
                    answer,
                };
                crate::jev::record_policy(self.execution_mode.load(), "jev_wait_skipped");
                self.apply_effort_fallback(cfg, "the turn's effort answer is not in yet");
            }
        }
    }

    /// On a round that keeps the turn's route under a once-per-turn cadence,
    /// once the turn's effort answer is in: it replaces the route's level
    /// where switching is free (the cache is not keyed by effort, or the
    /// prompt is still small); elsewhere it is dropped, so the late decision
    /// never costs an uncached re-read.
    pub(super) fn apply_late_turn_effort(&self, cfg: &mut SamplingConfig) {
        if self.round_effort_policy().cadence != EffortCadence::PerTurn {
            return;
        }
        let turn = self.current_turn_number.get();
        let (offered, answer) = {
            let mut ledger = self.jev_ledger.borrow_mut();
            match std::mem::take(&mut ledger.turn_effort) {
                TurnEffort::Pending {
                    turn: asked,
                    offered,
                    answer,
                } if asked == turn && answer.is_finished() => {
                    ledger.turn_effort = TurnEffort::Taken { turn };
                    (offered, answer)
                }
                other => {
                    ledger.turn_effort = other;
                    return;
                }
            }
        };
        let Some(answers) = finished_answer(answer) else {
            return;
        };
        let free = !effort_keys_cached_prefix(&cfg.api_backend, &cfg.model)
            || self
                .jev_ledger
                .borrow()
                .last_main_prompt_tokens()
                .is_none_or(|tokens| tokens < EFFORT_SWITCH_FREE_PROMPT_TOKENS);
        if !free {
            crate::jev::record_item(
                JevLever::B2MicroEffort,
                "defer",
                "the turn's effort answer came after its route was set on a large prompt",
                None,
                Some(&answers),
            );
            return;
        }
        self.apply_auto_route_effort(cfg, &answers, &offered, routing::MICRO_EFFORT_QUESTION);
        self.jev_ledger.borrow_mut().replace_turn_route(TurnRoute {
            model: cfg.model.clone(),
            effort: cfg.reasoning_effort,
        });
    }

    /// Sets this round's effort to `value`, on the model variant that serves it.
    fn set_round_effort(&self, cfg: &mut SamplingConfig, value: ReasoningEffort) {
        cfg.reasoning_effort = Some(value);
        if !self.child_model_routing_locked()
            && let Some(model) = self.models_manager.model_for_effort(&cfg.model, value)
        {
            cfg.model = model;
        }
    }

    /// The effort this round runs at when `cfg` names none.
    fn round_effort(&self, cfg: &SamplingConfig) -> Option<ReasoningEffort> {
        cfg.reasoning_effort
            .or_else(|| self.models_manager.current_reasoning_effort())
            .or_else(|| {
                self.models_manager
                    .model_default_reasoning_effort(&cfg.model)
            })
    }

    /// The row's ceiling, and its floor on the round after a failed tool or
    /// a failed recorded check, on this round's effort. Runs after the
    /// decisions and the redo floor and before the escalation ratchet's
    /// floor, so a climb is never capped. Where the cache is keyed by effort
    /// they act only on a round that sets the turn's route (`kept` is
    /// false), so they never force an uncached re-read mid-turn. An explicit
    /// effort turns them off.
    pub(super) fn apply_mode_effort_bounds(&self, cfg: &mut SamplingConfig, kept: bool) {
        if !self
            .jev_effort_auto
            .load(std::sync::atomic::Ordering::Relaxed)
            || (kept && effort_keys_cached_prefix(&cfg.api_backend, &cfg.model))
        {
            return;
        }
        let policy = self.round_effort_policy();
        let mode = self.execution_mode.load();
        if let Some(ceiling) = policy.ceiling.map(effort_level_value)
            && self
                .round_effort(cfg)
                .is_some_and(|current| effort_rank(current) > effort_rank(ceiling))
            && let Some(value) = self
                .models_manager
                .model_reasoning_efforts(&cfg.model)
                .into_iter()
                .map(|option| option.value)
                .filter(|value| effort_rank(*value) <= effort_rank(ceiling))
                .max_by_key(|value| effort_rank(*value))
        {
            self.set_round_effort(cfg, value);
            let mut ledger = self.jev_ledger.borrow_mut();
            // The route this round set keeps the capped level too.
            if !kept
                && ledger
                    .turn_route_effort()
                    .flatten()
                    .is_some_and(|effort| effort_rank(effort) > effort_rank(ceiling))
            {
                ledger.replace_turn_route(TurnRoute {
                    model: cfg.model.clone(),
                    effort: cfg.reasoning_effort,
                });
            }
            drop(ledger);
            crate::jev::record_policy(mode, "effort_ceiling");
        }
        if let Some(floor) = policy.floor_after_failure.map(effort_level_value)
            && self.jev_ledger.borrow().follows_failure()
            && self
                .round_effort(cfg)
                .is_none_or(|current| effort_rank(current) < effort_rank(floor))
            && self
                .models_manager
                .model_supports_reasoning_effort_value(&cfg.model, floor)
        {
            self.set_round_effort(cfg, floor);
            crate::jev::record_policy(mode, "effort_floor");
        }
    }

    /// The turn-start `<skill_relevance>` line. Awaited where the row waits
    /// for turn-start questions; otherwise started on a detached task and
    /// `None` here, and [`Self::flush_late_skill_hint`] appends it before the
    /// turn's second request if it is in by then.
    pub(super) async fn turn_start_skill_hint(self: &Arc<Self>, request: &str) -> Option<String> {
        if self.mode_policy().runtime.jev.await_turn_start {
            return self.jev_skill_relevance_hint(request).await;
        }
        // Nothing to rank: no question, so no wait to skip.
        if !crate::jev::lever_active(JevLever::P6SkillSuggestion)
            || request.trim().is_empty()
            || self.tool_bridge_handle().slash_skills().await.is_empty()
        {
            return None;
        }
        let actor = Arc::clone(self);
        let request = request.to_owned();
        let hint = tokio::task::spawn_local(crate::jev::in_current_turn(async move {
            actor.jev_skill_relevance_hint(&request).await
        }));
        let previous = self
            .jev_ledger
            .borrow_mut()
            .late_skill_hint
            .replace((self.current_turn_number.get(), hint));
        if let Some((_, previous)) = previous {
            previous.abort();
        }
        crate::jev::record_policy(self.execution_mode.load(), "jev_wait_skipped");
        None
    }

    /// Before the turn's second request (`loop_index` 2): appends the
    /// detached skill-relevance line as a reminder if it is in, else drops it.
    pub(super) fn flush_late_skill_hint(&self, loop_index: u32) {
        if loop_index < 2 {
            return;
        }
        let Some((turn, hint)) = self.jev_ledger.borrow_mut().late_skill_hint.take() else {
            return;
        };
        if turn != self.current_turn_number.get() || !hint.is_finished() {
            hint.abort();
            return;
        }
        if let Some(line) = futures::FutureExt::now_or_never(hint)
            .and_then(Result::ok)
            .flatten()
        {
            self.push_system_reminder(&line);
        }
    }

    /// The after-edit review (C4) waits at most the row's deadline; past it
    /// the edit goes on unreviewed, as when Jev times out.
    pub(super) async fn within_c4_deadline<T: Default>(
        &self,
        ask: impl std::future::Future<Output = T>,
    ) -> T {
        let deadline = self.mode_policy().runtime.jev.diff_risk_deadline_ms;
        if deadline >= distill_tool_types::JEV_DEADLINE_MS {
            return ask.await;
        }
        match tokio::time::timeout(std::time::Duration::from_millis(deadline), ask).await {
            Ok(answers) => answers,
            Err(_) => {
                crate::jev::record_policy(self.execution_mode.load(), "c4_deadline");
                T::default()
            }
        }
    }

    /// Whether the utility model may select what of a tool result reaches the
    /// next request: the selection lever is on and the mode's row allows it.
    /// The deterministic reducers (crushers, read reuse) never ask this. A row
    /// that turns selection off records it at most once per round.
    pub(super) fn utility_selection_active(&self) -> bool {
        if !crate::jev::lever_active(JevLever::ECheapCompress) {
            return false;
        }
        if self.mode_policy().runtime.utility_selection {
            return true;
        }
        let round = (
            self.current_turn_number.get(),
            crate::jev::telemetry_context().2,
        );
        let first = self.jev_ledger.borrow().utility_skip_recorded != Some(round);
        if first {
            self.jev_ledger.borrow_mut().utility_skip_recorded = Some(round);
            crate::jev::record_policy(self.execution_mode.load(), "utility_selection_skipped");
        }
        false
    }
}

#[cfg(test)]
#[path = "mode_effort_tests.rs"]
mod tests;
