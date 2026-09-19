// Modified for Distill by Samuel Fajreldines, 2026.
//! Turn cancellation, task and subagent kills, and overdue turn reconciliation.

use super::ctx::{active_subagent_view_mut, find_agent_by_session_id};
use super::permissions::drain_permission_queue;
use super::queue::{apply_turn_start_shim, maybe_drain_queue, note_peek_page_flip};
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::{ActivePane, AgentView};
use crate::app::app_view::{ActiveView, AppView};
use crate::app::cancel_latency::{CancelOrigin, TurnEnd};
use distill_telemetry::events::CancellationScope;
use std::time::Instant;

/// Map `[ui].cancel_subagents_on_turn_cancel` / in-memory agent preference to `cancel_subagents` for the cancel wire payload.
/// `None` means prompt.
fn effective_cancel_subagents_preference(
    agent_pref: Option<bool>,
    ui: &distill_shell::agent::config::UiConfig,
) -> Option<bool> {
    agent_pref.or(match ui.cancel_subagents_on_turn_cancel.as_deref() {
        Some("always_stop") => Some(true),
        Some("always_continue") => Some(false),
        _ => None,
    })
}

fn cancel_subagents_pref_canonical(stop: bool) -> &'static str {
    if stop {
        "always_stop"
    } else {
        "always_continue"
    }
}

fn cancel_subagents_pref_canonical_from_ui(
    ui: &distill_shell::agent::config::UiConfig,
) -> &'static str {
    match ui.cancel_subagents_on_turn_cancel.as_deref() {
        Some("always_stop") => "always_stop",
        Some("always_continue") => "always_continue",
        _ => "ask",
    }
}

/// Apply a global always-stop / always-continue preference to every agent and `app.current_ui`.
/// In-memory only; the caller emits `Effect::PersistSetting`.
pub(super) fn apply_cancel_subagents_preference_global(app: &mut AppView, stop: bool) {
    let canonical = cancel_subagents_pref_canonical(stop);
    app.current_ui.cancel_subagents_on_turn_cancel = Some(canonical.to_string());
    for agent in app.agents.values_mut() {
        agent.cancel_subagents_preference = Some(stop);
    }
}

pub(super) fn dispatch_cancel_turn(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    // Overlay [stop] is the child's turn. Parent may be Idle (background Task) and the ask panel would render under the overlay, unreachable.
    if let Some(agent) = active_subagent_view_mut(app) {
        // No wire target: leave local state alone (do not flip to Cancelling).
        let Some(session_id) = agent.session.session_id.clone() else {
            return vec![];
        };
        let retrying = agent.any_cancel_pending();
        crate::unified_log::info(
            if retrying {
                "cancel.retry"
            } else {
                "cancel.overlay"
            },
            Some(&session_id.0),
            Some(serde_json::json!({
                "current_prompt_id": agent.session.current_prompt_id,
            })),
        );
        if retrying {
            agent.clear_send_now_expectation();
            return vec![emit_cancel_turn(
                agent, session_id, /* cancel_subagents */ true,
                /* rewind_prompt_id */ None,
            )];
        }
        return cancel_agent_turn(
            agent,
            /* cancel_rewind_enabled */ false,
            /* cancel_subagents */ true,
            CancelOrigin::UserGesture,
        );
    }
    // Focused running subagent with no child view (no overlay to cancel through): kill is the same action as the row's kill button
    let focused_subagent_kill = app.agents.get(&id).and_then(|agent| {
        let child_sid = agent.active_subagent.as_ref()?;
        let info = agent.subagent_sessions.get(child_sid.as_str())?;
        info.is_running().then(|| info.subagent_id.to_string())
    });
    if let Some(subagent_id) = focused_subagent_kill {
        return dispatch_kill_subagent(app, subagent_id);
    }
    let ui_pref = effective_cancel_subagents_preference(None, &app.current_ui);

    // Scoped agent borrow: extract decisions, then release before `do_cancel_turn`.
    let preferred_cancel_subagents = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        let resolved_pref = agent.cancel_subagents_preference.or(ui_pref);
        // Retry path: a cancel was already sent (`TurnCancelling`) but the turn never resolved
        // Ctrl+C / palette CancelTurn is then never a dead key on a stuck "Cancelling…" spinner
        // A retry after a one-shot "Continue to run" must not escalate to killing the subagents the user chose to keep
        let resolve_cancel_subagents = |agent: &crate::app::agent_view::AgentView| {
            let target = agent
                .running_wake_turn
                .as_ref()
                .map(|wake| wake.prompt_id.clone())
                .or_else(|| agent.session.current_prompt_id.clone());
            agent
                .pending_cancel_resend
                .as_ref()
                .filter(|p| p.prompt_id == target)
                .map(|p| p.cancel_subagents)
                .or(resolved_pref)
                .unwrap_or(true)
        };
        if agent.session.state.is_cancelling() {
            let Some(session_id) = agent.session.session_id.clone() else {
                return vec![];
            };
            crate::unified_log::info(
                "cancel.retry",
                Some(&session_id.0),
                Some(serde_json::json!({
                    "current_prompt_id": agent.session.current_prompt_id,
                })),
            );
            // Explicit user cancel supersedes any pending send-now expectation (its marker renders).
            agent.clear_send_now_expectation();
            let cancel_subagents = resolve_cancel_subagents(agent);
            return vec![emit_cancel_turn(
                agent,
                session_id,
                cancel_subagents,
                /* rewind_prompt_id */ None,
            )];
        }
        // Compact owns the pane (`CommandRunning`) even if a leftover wake marker is still set; `/compact` can drain while that marker is live
        // This branch must beat the wake early-return or Esc never calls cancel_compact
        if agent.session.state.is_compact_running() {
            resolved_pref.or(Some(true))
        } else if agent.running_wake_turn.is_some() {
            // Keyed on the wake marker, not on an idle pane: a local send during a wake start_turn's the pane while the shell's front turn is still the wake
            // Cancel that wake; the queued user prompt must survive
            let Some(session_id) = agent.session.session_id.clone() else {
                return vec![];
            };
            agent.clear_send_now_expectation();
            let cancel_subagents = resolve_cancel_subagents(agent);
            agent.mark_wake_cancel_sent();
            return vec![emit_cancel_turn(
                agent,
                session_id,
                cancel_subagents,
                /* rewind_prompt_id */ None,
            )];
        } else if !agent.session.state.is_turn_running() {
            return vec![];
        } else if let Some(stop) = resolved_pref {
            Some(stop)
        } else {
            // Check all running subagents, not just those from the current turn.
            // This is broader than the old TUI (which filtered by parent_prompt_id), but intentional
            // Subagents kept alive from a previous cancel should still prompt the user on the next cancel
            let running_count = agent
                .subagent_sessions
                .values()
                .filter(|s| s.is_running() && s.attempt.workflow_run_id.is_none())
                .count();
            if running_count > 0 && agent.cancel_turn_view.is_none() {
                // Mandatory ingress wins: evict an open feedback modal before the cancel prompt takes input.
                agent.displace_feedback_modal(
                    crate::views::feedback_modal::FeedbackModalDisplacement::CancelTurn,
                );
                agent.cancel_turn_view = Some(crate::views::modal::CancelTurnViewState {
                    active_idx: 0,
                    running_count,
                });
                // Default focus to the picker so keyboard up/down navigates options immediately
                // With the scrollback pane focused (e.g. browsing history) the modal would open but keys would still go to scrollback.
                // The picker was then only reachable via mouse hover/click
                if agent.active_pane == ActivePane::Scrollback {
                    agent.active_pane = ActivePane::Prompt;
                }
                return vec![];
            }
            None
        }
    };

    do_cancel_turn(
        app,
        preferred_cancel_subagents.unwrap_or(true),
        CancelOrigin::UserGesture,
    )
}

pub(super) fn dispatch_cancel_turn_choice(
    app: &mut AppView,
    choice: crate::views::modal::CancelTurnChoice,
) -> Vec<Effect> {
    use crate::views::modal::CancelTurnChoice;
    let cancel_subagents = matches!(
        choice,
        CancelTurnChoice::StopRunning | CancelTurnChoice::AlwaysStop
    );

    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.cancel_turn_view = None;
        agent.cancel_turn_buttons.clear();
    }

    let mut effects = Vec::new();
    match choice {
        CancelTurnChoice::AlwaysStop | CancelTurnChoice::AlwaysContinue => {
            let stop = matches!(choice, CancelTurnChoice::AlwaysStop);
            let prev_canonical = cancel_subagents_pref_canonical_from_ui(&app.current_ui);
            let new_canonical = cancel_subagents_pref_canonical(stop);
            apply_cancel_subagents_preference_global(app, stop);
            if prev_canonical != new_canonical {
                tracing::info!(
                    target: "settings",
                    key = "cancel_subagents_on_turn_cancel",
                    value = new_canonical,
                    "setting changed",
                );
                effects.push(Effect::PersistSetting {
                    key: "cancel_subagents_on_turn_cancel",
                    value: crate::settings::SettingValue::Enum(new_canonical),
                    rollback_value: crate::settings::SettingValue::Enum(prev_canonical),
                });
            }
        }
        // One-shot choices: apply only to this cancel; global/session pref unchanged.
        CancelTurnChoice::StopRunning | CancelTurnChoice::ContinueToRun => {}
    }

    effects.extend(do_cancel_turn(
        app,
        cancel_subagents,
        CancelOrigin::UserGesture,
    ));
    effects
}

pub(super) fn do_cancel_turn(
    app: &mut AppView,
    cancel_subagents: bool,
    origin: CancelOrigin,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let cancel_rewind_enabled = app.cancel_rewind_enabled;
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    cancel_agent_turn(agent, cancel_rewind_enabled, cancel_subagents, origin)
}

fn cancel_agent_turn(
    agent: &mut AgentView,
    cancel_rewind_enabled: bool,
    cancel_subagents: bool,
    origin: CancelOrigin,
) -> Vec<Effect> {
    if agent.session.state.is_compact_running() {
        agent.cancel_and_arm(CancellationScope::Compaction, origin);
        agent.cancel_turn_view = None;
        agent.cancel_turn_buttons.clear();
        drain_permission_queue(agent);
        let Some(session_id) = agent.session.session_id.clone() else {
            return vec![];
        };
        agent.clear_send_now_expectation();
        return vec![emit_cancel_turn(
            agent,
            session_id,
            cancel_subagents,
            /* rewind_prompt_id */ None,
        )];
    }
    if agent.running_wake_turn.is_some() {
        let Some(session_id) = agent.session.session_id.clone() else {
            return vec![];
        };
        agent.clear_send_now_expectation();
        agent.mark_wake_cancel_sent();
        return vec![emit_cancel_turn(
            agent,
            session_id,
            cancel_subagents,
            /* rewind_prompt_id */ None,
        )];
    }
    if !agent.session.state.is_turn_running() {
        return vec![];
    }
    // The UI then looks like the user never hit Send
    // Minimal mode prints each committed block once into the terminal's native scrollback, and that print can't be "un-printed"
    // A user-prompt block commits immediately (it is never `is_running`)
    let in_flight_committed = match agent.session.in_flight_prompt.as_ref() {
        Some(stashed) => agent.scrollback.is_committed(stashed.scrollback_entry),
        None => false,
    };
    // The rewind REPLACES the composer with the stashed in-flight prompt.
    // Esc (and the mouse stop / palette cancel) fire with the draft intact, unlike keyboard Ctrl+C, which only cancels on an empty prompt
    // A non-empty composer thus holds a NEWER draft the rewind would clobber
    let composer_has_draft = !agent.prompt.text().is_empty() || !agent.prompt.images.is_empty();
    // Captured before `finish_turn` clears it; no id means the standard cancel
    let rewind_prompt_id = agent.session.current_prompt_id.clone();
    let queue_held_behind_turn = !agent.session.pending_prompts.is_empty()
        || agent
            .shared_queue
            .iter()
            .any(|e| Some(e.id.as_str()) != rewind_prompt_id.as_deref());
    let rewinding = cancel_rewind_enabled
        && agent.session.in_flight_prompt.is_some()
        && !queue_held_behind_turn
        && !in_flight_committed
        && !composer_has_draft
        && rewind_prompt_id.is_some();
    if rewinding
        && let Some(pid) = rewind_prompt_id.as_deref()
        && let Some(stashed) = agent.session.in_flight_prompt.take()
    {
        rewind_in_flight_prompt(agent, stashed, pid, RewindTarget::ReplaceComposer);
    } else {
        agent.cancel_and_arm(CancellationScope::Turn, origin);
    }
    agent.cancel_turn_view = None;
    agent.cancel_turn_buttons.clear();
    drain_permission_queue(agent);
    agent.dismiss_in_turn_plan_review();

    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    // Explicit user cancel supersedes any pending send-now expectation (its marker renders).
    agent.clear_send_now_expectation();

    // On an interactive cancel we only tear down the running turn and let the agent promote the FRONT queued prompt as the next turn
    // We do NOT pull any queued prompt back into the input or predict the new queue order client-side
    // `rewinding` mirrors the local rewind on the wire so the shell trims its stored copy too
    vec![emit_cancel_turn(
        agent,
        session_id,
        cancel_subagents,
        if rewinding { rewind_prompt_id } else { None },
    )]
}

/// Turn-end view teardown shared by every path that ends a turn: timing marker, stale permission
/// prompts (each gets Cancelled), the cancel panel, and the bash-mode focus reset.
pub(super) fn finish_turn_view(agent: &mut AgentView, end: TurnEnd) {
    agent.mark_turn_finished(end);
    agent.activity_started_at = None;
    agent.last_activity = None;
    drain_permission_queue(agent);
    agent.cancel_turn_view = None;
    agent.cancel_turn_buttons.clear();
    // After a bash-mode turn, scroll to bottom so the user sees the command output
    if agent.bash_turn {
        agent.bash_turn = false;
        agent.scrollback.goto_bottom();
    }
}

/// Where a rewound prompt's text goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RewindTarget {
    /// The composer is empty: restore text, chips, images, and cursor verbatim.
    ReplaceComposer,
    /// The composer holds a newer image-free draft: prepend the rewound text; the draft's chips fall back to raw text.
    MergeIntoDraft,
}

/// Pull a sent-but-unacknowledged prompt back into the composer and end its turn locally.
/// Records the prompt id as rewound so a late queue broadcast, update, or response for it is dropped by the existing gates.
pub(super) fn rewind_in_flight_prompt(
    agent: &mut AgentView,
    stashed: crate::app::agent::InFlightPrompt,
    rewind_prompt_id: &str,
    target: RewindTarget,
) {
    agent.note_rewound_prompt(rewind_prompt_id);
    match target {
        RewindTarget::ReplaceComposer => {
            agent.prompt.set_text(&stashed.text);
            agent.prompt.restore_chip_elements(&stashed.chip_elements);
            agent.prompt.set_images(stashed.images);
            agent.prompt.set_cursor(stashed.text.len());
        }
        RewindTarget::MergeIntoDraft => {
            let merged = format!("{}\n\n{}", stashed.text, agent.prompt.text());
            agent.prompt.set_text(&merged);
            // The rewound text is the prefix, so its chip ranges still hold; the draft's chips fall back to raw text
            agent.prompt.restore_chip_elements(&stashed.chip_elements);
            agent.prompt.set_images(stashed.images);
            agent.prompt.set_cursor(merged.len());
        }
    }
    // A block already printed into native scrollback (minimal mode) cannot be un-printed; the text still comes back
    for id in stashed
        .combined_scrollback_entries
        .into_iter()
        .chain([stashed.scrollback_entry])
    {
        if !agent.scrollback.is_committed(id) {
            agent.scrollback.remove_entry(id);
        }
    }
    agent.shared_queue.retain(|e| e.id != rewind_prompt_id);
    // Full state reset: tracker cleanup, state back to Idle, timing fields and current_prompt_id cleared
    agent.session.finish_turn(&mut agent.scrollback);
    agent.prompt_ack = None;
    agent.turn_started_at = None;
    agent.activity_started_at = None;
    agent.last_activity = None;
}

/// Build `Effect::CancelTurn`, consuming the gesture hint and arming the resend reconcile (skipped for a rewind, which leaves no cancelling state).
pub(super) fn emit_cancel_turn(
    agent: &mut crate::app::agent_view::AgentView,
    session_id: agent_client_protocol::SessionId,
    cancel_subagents: bool,
    rewind_prompt_id: Option<String>,
) -> Effect {
    let rewind_if_no_output = rewind_prompt_id.is_some();
    let target_prompt_id = if agent.session.state.is_compact_running()
        || matches!(
            &agent.session.state,
            crate::app::agent::AgentState::CommandCancelling { command }
                if command.is_compact()
        ) {
        agent.session.current_prompt_id.clone()
    } else {
        agent
            .running_wake_turn
            .as_ref()
            .map(|wake| wake.prompt_id.clone())
            .or_else(|| agent.session.current_prompt_id.clone())
    };
    // Prefer the live hint; a hint-less retry (palette) replays the recorded gesture so the shell still arms the wake barrier
    let trigger = agent
        .cancel_trigger_hint
        .take()
        .or_else(|| agent.pending_cancel_resend.as_ref().map(|p| p.trigger));
    // A local send during a wake adopts the user prompt while the shell's front turn is still the wake
    // Auto-resend has no prompt id on the wire, so arming it here would cancel the promoted user turn after the grace
    let desynced_from_wake = agent.running_wake_turn.as_ref().is_some_and(|wake| {
        agent
            .session
            .current_prompt_id
            .as_ref()
            .is_some_and(|pid| pid != &wake.prompt_id)
    });
    // Resend recovery is for user gestures; programmatic cancels (no trigger, e.g. login flows) own their retries.
    // A rewind leaves no cancelling state to key recovery on, so it never arms one either
    if let Some(trigger) = trigger
        && !rewind_if_no_output
        && !desynced_from_wake
    {
        // Keep `confirmed` across a manual retry: `[stop]` stays clickable while cancelling
        // Resetting the flag would re-arm auto-resend against a queued prompt the shell may already have promoted
        let existing = agent
            .pending_cancel_resend
            .as_ref()
            .filter(|p| p.prompt_id == target_prompt_id);
        let confirmed = existing.is_some_and(|p| p.confirmed);
        let attempts = existing.map(|p| p.attempts.max(1)).unwrap_or(1);
        agent.pending_cancel_resend = Some(crate::app::agent_view::PendingCancelResend {
            prompt_id: target_prompt_id,
            sent_at: Instant::now(),
            attempts,
            confirmed,
            cancel_subagents,
            trigger,
        });
    }
    Effect::CancelTurn {
        session_id,
        cancel_subagents,
        trigger,
        rewind_prompt_id,
    }
}

/// Grace before a still-cancelling pane re-sends its (fire-and-forget, loss-prone) `session/cancel`.
pub(crate) const CANCEL_RESEND_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Resend cap; past it a fresh gesture owns recovery.
pub(crate) const CANCEL_RESEND_MAX_ATTEMPTS: u8 = 3;

/// Re-send the (shell-idempotent) cancel for panes still in a cancelling state past [`CANCEL_RESEND_GRACE`].
/// Returns `None` when nothing fired.
pub(crate) fn reconcile_overdue_cancels(app: &mut AppView) -> Option<Vec<Effect>> {
    let mut effects = Vec::new();
    for agent in app.agents.values_mut() {
        if let Some(effect) = overdue_cancel_for_agent(agent) {
            effects.push(effect);
        }
        for child in agent.subagent_views.values_mut() {
            if let Some(effect) = overdue_cancel_for_agent(child) {
                effects.push(effect);
            }
        }
    }
    (!effects.is_empty()).then_some(effects)
}

fn overdue_cancel_for_agent(agent: &mut AgentView) -> Option<Effect> {
    // A cancelled wake turn keeps the pane Idle (never adopted), so its cancelling phase lives on `running_wake_turn` instead of the state
    if !agent.any_cancel_pending() {
        // The turn resolved (or a new one adopted); the marker is stale.
        agent.pending_cancel_resend = None;
        return None;
    }
    let session_id = agent.session.session_id.clone()?;
    // A received `prompt_complete` broadcast proves the cancel landed; the turn-end reconcile owns the exit from here
    // Resending would race it and could cancel a queued prompt the shell has already promoted
    if agent.pending_turn_end_reconcile.is_some() {
        if let Some(pending) = agent.pending_cancel_resend.as_mut() {
            pending.confirmed = true;
        }
        return None;
    }
    let pending = agent.pending_cancel_resend.as_mut()?;
    if pending.confirmed
        || pending.attempts >= CANCEL_RESEND_MAX_ATTEMPTS
        || pending.sent_at.elapsed() < CANCEL_RESEND_GRACE
    {
        return None;
    }
    pending.attempts += 1;
    pending.sent_at = Instant::now();
    crate::unified_log::warn(
        "cancel.resend",
        Some(&session_id.0),
        Some(serde_json::json!({
            "attempts": pending.attempts,
            "target_prompt_id": pending.prompt_id,
        })),
    );
    Some(Effect::CancelTurn {
        session_id,
        cancel_subagents: pending.cancel_subagents,
        trigger: Some(pending.trigger),
        rewind_prompt_id: None,
    })
}

/// Grace window between a driver-side `x.ai/session/prompt_complete` broadcast and that turn's `session/prompt` RPC response.
/// Past it, [`reconcile_overdue_turn_ends`] finishes the turn from the broadcast.
/// The healthy-path gap is milliseconds (the shell emits the broadcast just before writing the RPC response).
pub(crate) const TURN_END_RECONCILE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Finish turns whose end was announced by `x.ai/session/prompt_complete` but whose `session/prompt` RPC response never arrived.
/// The RPC response is the driver's only turn-state exit, and it can be lost in leader response routing / reconnect races.
/// The loss left the TUI latched in `TurnCancelling` until a restart (Esc dead, prompts piling into a queue that never drains).
pub(crate) fn reconcile_overdue_turn_ends(app: &mut AppView) -> Option<Vec<Effect>> {
    let overdue: Vec<AgentId> = app
        .agents
        .iter()
        .filter(|(_, a)| {
            a.pending_turn_end_reconcile
                .as_ref()
                .is_some_and(|p| p.received_at.elapsed() >= TURN_END_RECONCILE_GRACE)
        })
        .map(|(id, _)| *id)
        .collect();
    if overdue.is_empty() {
        return None;
    }

    let mut fired = false;
    let mut effects = Vec::new();
    let mut drained_ids = Vec::new();
    for id in overdue {
        // Take the stashed adoption before borrowing the agent (disjoint `app` fields; same pattern as the PromptResponse arm)
        let pending_adoption = app.pending_running_adoptions.remove(&id);
        let Some(agent) = app.agents.get_mut(&id) else {
            continue;
        };
        let Some(pending) = agent.pending_turn_end_reconcile.take() else {
            continue;
        };

        let still_ours =
            agent.session.current_prompt_id.as_deref() == Some(pending.prompt_id.as_str());
        let busy = agent.session.state.is_turn_running() || agent.session.state.is_cancelling();
        if !still_ours || !busy {
            // The turn already resolved through the normal path (or a new turn was adopted); the marker is stale
            // Restore the adoption for the path that owns it
            if let Some(p) = pending_adoption {
                app.pending_running_adoptions.insert(id, p);
            }
            continue;
        }

        fired = true;
        let was_cancelling = agent.session.state.is_cancelling()
            || pending.stop_reason.as_deref() == Some("cancelled");
        // Send-now cancel: suppress the marker (wire `cancelTrigger` wins, else the armed expectation)
        // Consumed every reconcile (no stale flag)
        let expected_send_now = agent.expect_send_now_cancel.take();
        let send_now_cancel = was_cancelling
            && match pending.cancel_trigger.as_deref() {
                Some(trigger) => trigger == "send_now",
                None => expected_send_now.is_some(),
            };
        let elapsed = agent.turn_elapsed();
        crate::unified_log::warn(
            "turn.end_reconciled_from_broadcast",
            agent.session.session_id.as_ref().map(|s| s.0.as_ref()),
            Some(serde_json::json!({
                "prompt_id": pending.prompt_id,
                "stop_reason": pending.stop_reason,
                "was_cancelling": was_cancelling,
                "send_now_cancel": send_now_cancel,
                "grace_ms": TURN_END_RECONCILE_GRACE.as_millis() as u64,
            })),
        );

        // Before `finish_turn`: the blocked-prompt requeue reads `in_flight_prompt`, which finish_turn clears
        crate::app::turn_completion::note_hook_blocked_turn(
            agent,
            Some(pending.prompt_id.as_str()),
            pending.cancellation_category.as_deref(),
            pending.cancellation_context.as_ref(),
        );
        agent.session.finish_turn(&mut agent.scrollback);
        let elapsed_ms = crate::app::turn_completion::duration_to_elapsed_ms(elapsed);
        let stop = if was_cancelling {
            crate::app::turn_completion::TurnStopReason::Cancelled
        } else {
            crate::app::turn_completion::TurnStopReason::from(pending.stop_reason.as_deref())
        };
        let event = crate::app::turn_completion::terminal_marker(
            crate::app::turn_completion::TerminalMarkerInput {
                stop,
                elapsed_ms,
                agent_result: pending.agent_result.as_deref(),
                send_now_cancel,
                cancel_trigger: pending.cancel_trigger.as_deref(),
                cancellation_category: pending.cancellation_category.as_deref(),
                error_kind: pending.error_kind,
                error_banner_present: !was_cancelling
                    && crate::app::dispatch::scrollback_has_recent_error_banner(&agent.scrollback),
            },
        );
        crate::app::turn_completion::push_turn_terminal_marker(agent, event);
        finish_turn_view(agent, TurnEnd::Completed);
        // Lost PromptResponse: do not leave execute_plan_prompt_id blocking review.
        agent.take_execute_plan_prompt(Some(pending.prompt_id.as_str()));

        // FIFO handoff (mirrors the PromptResponse arm): adopt the next server-authoritative running prompt now that the slot is free
        let adopted_page_flip = if let Some(p) = pending_adoption
            && agent.session.current_prompt_id.is_none()
        {
            if p.prompt_id != pending.prompt_id && agent.should_adopt_running_prompt(&p.prompt_id) {
                apply_turn_start_shim(agent, p.prompt_id, p.text, &p.kind, p.combined_texts)
            } else {
                agent.discard_pending_adoption_updates(&p.prompt_id);
                None
            }
        } else {
            None
        };
        let drain = maybe_drain_queue(agent);
        effects.extend(drain.effects);
        drained_ids.push((id, adopted_page_flip.or(drain.page_flip_entry)));
    }
    for (id, page_flip_entry) in drained_ids {
        note_peek_page_flip(app, id, page_flip_entry);
    }
    fired.then_some(effects)
}

pub(super) fn dispatch_cancel_scheduled_task(app: &mut AppView, task_id: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    // Remove from local state immediately (optimistic).
    agent.session.scheduled_tasks.remove(&task_id);

    vec![Effect::DeleteScheduledTask {
        session_id,
        task_id,
    }]
}

pub(super) fn dispatch_kill_bg_task(app: &mut AppView, task_id: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    // Mark as pending_kill for UI feedback
    if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
        task.pending_kill = true;
        task.kill_requested_at = Some(Instant::now());
    }

    vec![Effect::KillBgTask {
        session_id,
        task_id,
        source: distill_shell::extensions::task::TaskKillSource::ClientUi,
    }]
}

pub(super) fn dispatch_kill_subagent(app: &mut AppView, subagent_id: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    let attempt_id = agent
        .subagent_sessions
        .values_mut()
        .find(|info| info.subagent_id.as_ref() == subagent_id)
        .and_then(|info| {
            info.attempt.pending_kill = true;
            info.attempt.kill_requested_at = Some(Instant::now());
            info.attempt
                .lifecycle
                .current_attempt_id()
                .map(str::to_owned)
        });

    vec![Effect::KillSubagent {
        session_id,
        subagent_id,
        attempt_id,
    }]
}

pub(super) fn dispatch_demote_to_background(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if !agent.session.state.is_turn_running() {
        return vec![];
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    let Some(tool_call_id) = agent
        .session
        .tracker
        .running_execute_tool_call_id()
        .map(|s| s.to_string())
    else {
        return vec![];
    };

    tracing::info!(tool_call_id = %tool_call_id, "Demoting execute tool to background");

    vec![Effect::DemoteToBackground {
        session_id,
        tool_call_id,
    }]
}

// TaskResult handlers.

pub(super) fn handle_bg_task_killed(
    app: &mut AppView,
    session_id: String,
    task_id: String,
    outcome: Option<distill_tools::types::KillOutcome>,
) -> Vec<Effect> {
    use distill_tools::types::KillOutcome;
    if let Some(agent) = find_agent_by_session_id(&mut app.agents, &session_id) {
        match outcome {
            Some(KillOutcome::Killed) => {
                // Stay in pending_kill state; task_completed notification will arrive and clear it
                tracing::info!(task_id = %task_id, "Kill signal sent");
            }
            Some(KillOutcome::AlreadyExited) => {
                if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
                    task.pending_kill = false;
                    task.kill_requested_at = None;
                }
            }
            Some(KillOutcome::NotFound) => {
                // Stale row (restored from a resume replay but the process belongs to a previous session lifetime): the agent has nothing to kill
                // Drop the row and finish its "Task started" scrollback entry (stops the running accent that the replay restore turned on)
                tracing::info!(task_id = %task_id, "Task not found, removing");
                if let Some(task) = agent.session.bg_tasks.remove(&task_id)
                    && let Some(entry_id) = task.scrollback_entry_id
                {
                    agent.scrollback.finish_running(entry_id);
                }
            }
            None => {
                // Error envelope or unparseable payload: clear the pending state so the user can retry, keep the row
                tracing::warn!(task_id = %task_id, "Kill outcome missing or unparseable");
                if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
                    task.pending_kill = false;
                    task.kill_requested_at = None;
                }
            }
        }
    }
    vec![]
}
