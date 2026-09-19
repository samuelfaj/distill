// Modified for Distill by Samuel Fajreldines, 2026.
//! Fail-safe for a prompt the agent never acknowledged: abort the turn locally, put the text back, tell the shell.
//!
//! Runs from the event loop's animation tick like the other `reconcile_overdue_*` recoveries.
//! It never adopts, re-sends, or drains: the pane ends Idle with the prompt in the composer and the
//! prompt id recorded as rewound, so any late acknowledgment for it is dropped by the existing gates.

use super::turn::{RewindTarget, emit_cancel_turn, finish_turn_view, rewind_in_flight_prompt};
use crate::app::actions::Effect;
use crate::app::agent::AgentState;
use crate::app::agent_view::{AgentView, PromptMode};
use crate::app::app_view::AppView;
use crate::app::cancel_latency::TurnEnd;
use crate::app::prompt_ack::{PromptAckDeadlines, PromptAckOutcome};
use crate::scrollback::block::RenderBlock;
use distill_telemetry::events::{
    PromptAckDisposition, PromptAckPromptKind, PromptAckSurface, PromptAckTimeoutFired,
};
use std::time::{Duration, Instant};

const PROMPT_ACK_TIMEOUT_TOAST_RESTORED: &str = "Prompt not accepted, text restored";
const PROMPT_ACK_TIMEOUT_TOAST_STOPPED: &str = "Prompt not accepted, turn stopped";

/// Scrollback notice for a fired fail-safe; the wording names where the user's text went.
fn prompt_ack_timeout_notice(limit: Duration, disposition: PromptAckDisposition) -> String {
    let limit_secs = limit.as_secs();
    let recovery = match disposition {
        PromptAckDisposition::RestoredToComposer => {
            "Your text is back in the input box; press Enter to retry."
        }
        PromptAckDisposition::MergedIntoDraft => {
            "Your text was placed above your draft in the input box; press Enter to retry."
        }
        PromptAckDisposition::NotRestorable => {
            "The turn was stopped; send the prompt again to retry."
        }
    };
    format!(
        "The agent did not accept your prompt within {limit_secs}s. {recovery} If this keeps happening, run /feedback."
    )
}

/// Poll every armed acknowledgment watch: log the soft notice once, and past the hard deadline abort the turn.
/// Returns `None` when nothing changed; `Some(effects)` (possibly empty) when a notice or abort needs a redraw.
pub(crate) fn reconcile_overdue_prompt_acks(
    app: &mut AppView,
    deadlines: &PromptAckDeadlines,
) -> Option<Vec<Effect>> {
    reconcile_overdue_prompt_acks_at(app, deadlines, Instant::now())
}

/// [`reconcile_overdue_prompt_acks`] against an injected clock.
pub(super) fn reconcile_overdue_prompt_acks_at(
    app: &mut AppView,
    deadlines: &PromptAckDeadlines,
    now: Instant,
) -> Option<Vec<Effect>> {
    // A leader reconnect replays or aborts the turn itself; firing under it would race the reload finalize
    if app.reconnect_pending {
        return None;
    }
    let mut fired = false;
    let mut effects = Vec::new();
    // A prompt sent from a focused subagent overlay arms the watch on the child view, not the parent
    for agent in app.agents.values_mut() {
        fired |= poll_prompt_ack_for_agent(agent, deadlines, now, &mut effects);
        for child in agent.subagent_views.values_mut() {
            fired |= poll_prompt_ack_for_agent(child, deadlines, now, &mut effects);
        }
    }
    fired.then_some(effects)
}

/// One view's watch; returns whether it logged or fired.
fn poll_prompt_ack_for_agent(
    agent: &mut AgentView,
    deadlines: &PromptAckDeadlines,
    now: Instant,
    effects: &mut Vec<Effect>,
) -> bool {
    let Some(watch) = agent.prompt_ack.as_mut() else {
        return false;
    };
    // Stale guard: only the live turn's own watch may fire; anything else is dropped without effect
    // Cancelling stays armed so an Esc during the wedge cannot park the pane on "Cancelling…" forever
    let busy = matches!(
        agent.session.state,
        AgentState::TurnRunning | AgentState::TurnCancelling
    );
    let owns_turn = agent.session.current_prompt_id.as_deref() == Some(watch.prompt_id());
    if !busy || !owns_turn {
        agent.prompt_ack = None;
        return false;
    }
    let session_id = agent.session.session_id.clone();
    match watch.poll(now, deadlines) {
        PromptAckOutcome::Waiting => false,
        PromptAckOutcome::SoftNotice { waited } => {
            crate::unified_log::write_direct_warn(
                "prompt.ack_soft_notice",
                session_id.as_ref().map(|s| s.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": watch.prompt_id(),
                    "waited_ms": waited.as_millis() as u64,
                    "limit_ms": deadlines.hard.as_millis() as u64,
                })),
            );
            true
        }
        PromptAckOutcome::Expired { waited } => {
            agent.prompt_ack = None;
            effects.extend(fire_fail_safe(agent, waited, deadlines));
            true
        }
    }
}

/// Where the unacknowledged prompt's text can go, decided before the turn is torn down.
pub(super) fn restore_target(agent: &AgentView) -> Option<RewindTarget> {
    // The composer may be editing a queued row; writing the prompt into that edit would corrupt the row
    if agent.prompt_mode != PromptMode::Normal {
        return None;
    }
    let has_text = !agent.prompt.text().is_empty();
    let has_images = !agent.prompt.images.is_empty();
    match (has_text, has_images) {
        (false, false) => Some(RewindTarget::ReplaceComposer),
        (true, false) => Some(RewindTarget::MergeIntoDraft),
        // Both prompt lifetimes number images from 1, so the stash's `[Image #N]` placeholders would collide with the draft's
        (_, true) => None,
    }
}

/// Abort the unacknowledged turn: restore the text, end the turn as aborted, notify, and send a rewind cancel.
/// Order matters: `finish_turn` clears `in_flight_prompt` and `current_prompt_id`, so both are captured first.
fn fire_fail_safe(
    agent: &mut AgentView,
    waited: Duration,
    deadlines: &PromptAckDeadlines,
) -> Option<Effect> {
    let prompt_id = agent.session.current_prompt_id.clone()?;
    let session_id = agent.session.session_id.clone();
    let was_cancelling = agent.session.state.is_cancelling();
    let prompt_kind = if agent.bash_turn {
        PromptAckPromptKind::Bash
    } else if agent.session.in_flight_prompt.is_some() {
        PromptAckPromptKind::Prompt
    } else {
        PromptAckPromptKind::Skill
    };
    let disposition = match (agent.session.in_flight_prompt.take(), restore_target(agent)) {
        (Some(stashed), Some(target)) => {
            rewind_in_flight_prompt(agent, stashed, &prompt_id, target);
            match target {
                RewindTarget::ReplaceComposer => PromptAckDisposition::RestoredToComposer,
                RewindTarget::MergeIntoDraft => PromptAckDisposition::MergedIntoDraft,
            }
        }
        // No composer stash (skill, wire blocks, bash) or no safe composer to restore into: the user block stays for copying
        (None, _) | (Some(_), None) => {
            agent.note_rewound_prompt(&prompt_id);
            agent.session.finish_turn(&mut agent.scrollback);
            PromptAckDisposition::NotRestorable
        }
    };
    finish_turn_view(agent, TurnEnd::Aborted);
    // The turn is gone locally; a resend record for it would only re-cancel a prompt the shell never ran
    agent.pending_cancel_resend = None;

    agent
        .scrollback
        .push_block(RenderBlock::system(prompt_ack_timeout_notice(
            deadlines.hard,
            disposition,
        )));
    agent.show_toast(match disposition {
        PromptAckDisposition::RestoredToComposer | PromptAckDisposition::MergedIntoDraft => {
            PROMPT_ACK_TIMEOUT_TOAST_RESTORED
        }
        PromptAckDisposition::NotRestorable => PROMPT_ACK_TIMEOUT_TOAST_STOPPED,
    });

    crate::unified_log::write_direct_warn(
        "prompt.ack_timeout",
        session_id.as_ref().map(|s| s.0.as_ref()),
        Some(serde_json::json!({
            "prompt_id": prompt_id,
            "waited_ms": waited.as_millis() as u64,
            "limit_ms": deadlines.hard.as_millis() as u64,
            "prompt_kind": prompt_kind,
            "disposition": disposition,
            "was_cancelling": was_cancelling,
            "queue_depth": agent.session.pending_prompts.len(),
            "shared_queue_len": agent.shared_queue.len(),
        })),
    );
    distill_telemetry::session_ctx::log_event(PromptAckTimeoutFired {
        limit_ms: deadlines.hard.as_millis() as u64,
        waited_ms: waited.as_millis() as u64,
        surface: PromptAckSurface::Tui,
        disposition,
        prompt_kind,
    });

    // Not a user cancel: no gesture, so no resend record; the shell trims the prompt if it lands late
    Some(emit_cancel_turn(
        agent,
        session_id?,
        /* cancel_subagents */ false,
        Some(prompt_id),
    ))
}
