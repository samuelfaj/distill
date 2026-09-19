// Modified for Distill by Samuel Fajreldines, 2026.
//! Prompt-queue dispatch: the server-authoritative immediate-send routing helpers and the optimistic queue echoes.
//! It also holds the local drip-feed drain ([`maybe_drain_queue`]), the turn-start shim, and the queue-interject action arm.

use super::ctx::{NO_SESSION_NOTICE, active_agent_session_id, with_active_agent};
use crate::acp::meta::user_prompt_meta;
use crate::app::actions::Effect;
use crate::app::agent::{AgentCommand, AgentId};
use crate::app::agent_view::{AgentView, PromptMode};
use crate::app::app_view::{ActiveView, AppView};
use crate::app::command_catalog::CommandCatalogSource;
use crate::scrollback::EntryId;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::{MemoryCommandKind, SessionEvent};
use crate::scrollback::state::ScrollbackState;
use agent_client_protocol as acp;
use std::time::Instant;

fn page_flip_on_send() -> bool {
    crate::appearance::cache::load_page_flip_on_send()
}

/// Slash output skips the prompt drain; pin it at the top when page-flip is on.
pub(super) fn push_and_page_flip(scrollback: &mut ScrollbackState, block: RenderBlock) {
    scrollback.push_block(block);
    if !page_flip_on_send() {
        return;
    }
    let idx = scrollback.len() - 1;
    scrollback.page_flip_to_entry(idx);
}

fn combine_queued_prompts_enabled() -> bool {
    crate::appearance::cache::load_combine_queued_prompts()
}

/// We must also not be mid-edit, mid-model-switch, or mid-replay.
/// **Server-busy (`is_turn_running() || !shared_queue.is_empty()`):** the immediate-send path is for prompts that must queue server-side rather than start a turn locally.
/// **FIFO guard (`pending_prompts.is_empty()`):** a prompt may only jump onto the server queue when the local drip-feed queue is empty.
pub(super) fn immediate_server_send_eligible(agent: &AgentView, leader_mode: bool) -> bool {
    let wake_running = agent.running_wake_turn.is_some();
    let server_busy =
        agent.session.state.is_turn_running() || wake_running || !agent.shared_queue.is_empty();
    (leader_mode || crate::appearance::cache::load_follow_up_steer() || wake_running)
        && server_busy
        && agent.session.session_id.is_some()
        && agent.session.pending_prompts.is_empty()
        && !matches!(agent.prompt_mode, PromptMode::EditingQueued { .. })
        && !agent.session.model_switch_pending
        && !agent.session.loading_replay
}

/// Push the optimistic shared-queue echo for an immediate server-authoritative send and mirror it into the owning agent.
/// The queue pane then renders it immediately, before the confirming `x.ai/queue/changed` broadcast.
pub(super) fn push_server_queue_echo(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: &str,
    prompt_id: &str,
    text: &str,
    kind: &str,
) {
    app.push_optimistic_prompt_echo(session_id, prompt_id, text, kind);
    let snapshot = app
        .shared_prompt_queue(session_id)
        .cloned()
        .unwrap_or_default();
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.shared_queue = snapshot;
        // Track the unconfirmed echo so a queue-row send-now against it is parked until the confirming broadcast
        // See `AgentView::send_now_awaiting_confirm`
        agent.optimistic_queue_ids.insert(prompt_id.to_string());
    }
}

/// That covers a prompt restored on cancel, removed, drained, or otherwise resolved without becoming the running turn.
/// The only client-side queue state is the optimistic echo that bridges the round-trip before the confirming `x.ai/queue/changed` broadcast.
/// Once a prompt's RPC resolves (or we pull it back on cancel) it will never reappear in a future broadcast, so its echo must be dropped.
pub(super) fn retire_optimistic_echo(
    optimistic: &mut std::collections::HashMap<
        String,
        Vec<crate::app::prompt_queue::QueueEntryWire>,
    >,
    shared: &mut std::collections::HashMap<String, Vec<crate::app::prompt_queue::QueueEntryWire>>,
    session_id: &str,
    prompt_id: &str,
) {
    if let Some(opt) = optimistic.get_mut(session_id) {
        opt.retain(|e| e.id != prompt_id);
        if opt.is_empty() {
            optimistic.remove(session_id);
        }
    }
    if let Some(q) = shared.get_mut(session_id) {
        q.retain(|e| e.id != prompt_id);
        if q.is_empty() {
            shared.remove(session_id);
        }
    }
}

/// Drain prompt-side images and snapshot all chip elements (paste blocks, @-file refs, image chips) into the most recently enqueued `QueuedPrompt`.
/// Must be called after `enqueue_prompt` / `push_back` and before `prompt.set_text("")` (which clears element and image state).
/// If the last queued entry already has `wire_blocks` (skill injection), images are dropped with a toast instead of merged.
pub(super) fn drain_prompt_state_to_last_queued(agent: &mut AgentView) {
    let prompt_state = agent.prompt.stash();
    let (_, images, chip_elements) = prompt_state.into_submission();

    let Some(entry) = agent.session.pending_prompts.back_mut() else {
        return;
    };

    // Injected skill text can be shorter than the composer that supplied the chips.
    let text = entry.text.as_str();
    entry.chip_elements = chip_elements
        .into_iter()
        .filter(|chip| text.get(chip.range.clone()).is_some())
        .collect();

    if images.is_empty() {
        return;
    }

    // wire_blocks policy: skill-injected prompts do not carry prompt images.
    if entry.wire_blocks.is_some() {
        agent.show_toast("Images removed (skill prompt)");
        return;
    }

    entry.images = images;
}

/// Try to send the next queued entry (prompt, command, or bash) if the agent is idle.
/// Called after enqueue operations and task completions to advance the queue.
/// **Prompt**: pushes user prompt block to scrollback, starts turn, returns `Effect::SendPrompt`
pub(super) struct QueueDrain {
    pub(super) effects: Vec<Effect>,
    pub(super) page_flip_entry: Option<EntryId>,
}

impl QueueDrain {
    fn blocked() -> Self {
        Self {
            effects: Vec::new(),
            page_flip_entry: None,
        }
    }
}

/// Same as [`flush_held_local_queue_into_wait`].
pub(crate) fn maybe_release_queued_prompt_into_turn(
    app: &mut AppView,
    agent_id: Option<AgentId>,
) -> Vec<Effect> {
    flush_held_local_queue_into_wait(app, agent_id)
}

fn flush_target_id(app: &AppView, agent_id: Option<AgentId>) -> Option<AgentId> {
    agent_id.or(match app.active_view {
        ActiveView::Agent(id) => Some(id),
        _ => None,
    })
}

/// Inject held local follow-ups into a parked wait, oldest first.
/// A leftover earlier row would otherwise resend after the wait.
pub(crate) fn flush_held_local_queue_into_wait(
    app: &mut AppView,
    agent_id: Option<AgentId>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    let mut skipped = Vec::new();
    loop {
        let released = release_queued_prompt_from(app, agent_id);
        if !released.is_empty() {
            effects.extend(released);
            continue;
        }
        let Some(id) = flush_target_id(app, agent_id) else {
            break;
        };
        let Some(agent) = app.agents.get_mut(&id) else {
            break;
        };
        if matches!(agent.prompt_mode, PromptMode::EditingQueued { .. })
            || agent.session.pending_prompts.is_empty()
        {
            break;
        }
        let front_unflushable = agent.session.pending_prompts.front().is_some_and(|p| {
            !matches!(p.kind, crate::app::agent::QueueEntryKind::Prompt)
                || !p.wire_matches_display()
        });
        if !front_unflushable {
            break;
        }
        if let Some(row) = agent.session.pending_prompts.pop_front() {
            skipped.push(row);
        }
    }
    if let Some(id) = flush_target_id(app, agent_id)
        && let Some(agent) = app.agents.get_mut(&id)
    {
        for row in skipped.into_iter().rev() {
            agent.session.pending_prompts.push_front(row);
        }
    }
    effects
}

/// Flush held follow-ups, then run `then`.
pub(super) fn with_held_queue_flush(
    app: &mut AppView,
    then: impl FnOnce(&mut AppView) -> Vec<Effect>,
) -> Vec<Effect> {
    let mut effects = flush_held_local_queue_into_wait(app, None);
    effects.extend(then(app));
    effects
}

fn release_queued_prompt_from(app: &mut AppView, agent_id: Option<AgentId>) -> Vec<Effect> {
    use crate::app::agent::QueueEntryKind;

    if !crate::appearance::cache::load_follow_up_steer() {
        return Vec::new();
    }

    let id = match agent_id {
        Some(id) => id,
        None => match app.active_view {
            ActiveView::Agent(id) => id,
            _ => return Vec::new(),
        },
    };
    let Some(agent) = app.agents.get(&id) else {
        return Vec::new();
    };
    if !agent.session.state.is_turn_running() || agent.session.session_id.is_none() {
        return Vec::new();
    }
    // Mid-outage: the interject effect has no requeue path, so a row released into a dead channel is simply lost
    // Leave it queued
    if app.reconnect_pending {
        return Vec::new();
    }
    if !agent.is_parked_on_sendable_wait()
        || agent.session.loading_replay
        || agent.expect_send_now_cancel.is_some()
    {
        return Vec::new();
    }
    // Merged steer order is server-first; do not interject a local row ahead of a held server row.
    let running = agent.session.current_prompt_id.as_deref();
    let send_now = agent.expect_send_now_cancel.as_deref();
    if agent.shared_queue.iter().any(|entry| {
        crate::views::queue_pane::visible_held_server_row(
            &entry.id,
            running,
            send_now,
            &agent.send_now_painted_blocks,
        )
    }) {
        return Vec::new();
    }
    if matches!(agent.prompt_mode, PromptMode::EditingQueued { .. })
        || !agent
            .session
            .pending_prompts
            .front()
            .is_some_and(|p| matches!(p.kind, QueueEntryKind::Prompt) && p.wire_matches_display())
    {
        return Vec::new();
    }

    let Some(agent) = app.agents.get_mut(&id) else {
        return Vec::new();
    };
    let Some(queued) = agent.session.pending_prompts.pop_front() else {
        return Vec::new();
    };
    crate::unified_log::info(
        "prompt.release_into_turn",
        agent.session.session_id.as_ref().map(|s| s.0.as_ref()),
        Some(serde_json::json!({
            "remaining_in_queue": agent.session.pending_prompts.len(),
            "prompt_len": queued.text.len(),
        })),
    );
    super::interject::dispatch_interject_from_held_queue(app, id, queued.text, queued.images)
}

pub(super) fn maybe_drain_queue(agent: &mut AgentView) -> QueueDrain {
    use crate::app::agent::QueueEntryKind;
    use crate::unified_log as ulog;

    let sid = agent.session.session_id.as_ref().map(|s| s.0.as_ref());
    let queue_depth = agent.session.pending_prompts.len();

    let log_blocked = |reason: &str, sid: Option<&str>| {
        if queue_depth > 0 {
            ulog::debug(
                "prompt.drain_blocked",
                sid,
                Some(serde_json::json!({"reason": reason, "queue_depth": queue_depth})),
            );
        }
    };

    if !agent.session.state.is_idle() {
        log_blocked("turn_running", sid);
        return QueueDrain::blocked();
    }
    // The pane stays Idle around a non-adopted wake; draining here would
    // `start_turn` a fake local turn that the shell will only queue.
    if agent.running_wake_turn.is_some() {
        log_blocked("wake_turn_running", sid);
        return QueueDrain::blocked();
    }
    // Hold the drain during an in-flight model switch
    // See the `model_switch_pending` field doc for why a reconnect must clear it
    if agent.session.model_switch_pending {
        log_blocked("model_switch_pending", sid);
        return QueueDrain::blocked();
    }
    if agent.session.loading_replay {
        log_blocked("loading_replay", sid);
        return QueueDrain::blocked();
    }
    // A hook blocked the previous prompt: park the local drip-feed queue until the user re-engages (see `hook_block_hold`)
    if agent.session.hook_block_hold {
        log_blocked("hook_block_hold", sid);
        return QueueDrain::blocked();
    }
    // Server-owned next turn: a non-running server row (including this client's own in-flight send-now echo) drains shell-side
    // Draining a local row now would optimistically promote it as the running turn while the shell runs the server row
    // The server row's deltas then fail the prompt-id gate and render nothing (the FIFO invariant documented on `immediate_server_send_eligible`)
    let running = agent.session.current_prompt_id.as_deref();
    if agent
        .shared_queue
        .iter()
        .any(|e| Some(e.id.as_str()) != running)
    {
        log_blocked("server_queue_owns_next_turn", sid);
        return QueueDrain::blocked();
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        log_blocked("no_session_id", None);
        return QueueDrain::blocked();
    };

    // Block drain if the user is editing the front prompt.
    if let PromptMode::EditingQueued { id, .. } = &agent.prompt_mode
        && agent
            .session
            .pending_prompts
            .front()
            .is_some_and(|p| p.id == *id)
    {
        // The prompt being edited is next to send; don't drain it from under the user
        // The turn status line will show a "waiting on your edit" indicator
        log_blocked("user_editing_front", Some(&session_id.0));
        return QueueDrain::blocked();
    }

    // Row the user is actively editing (if any)
    // The front-row case is already handled above; pass it so a combined drain also stops before an edited *follower* instead of merging it away
    let editing_id = match &agent.prompt_mode {
        PromptMode::EditingQueued { id, .. } => Some(*id),
        _ => None,
    };
    let queued = match if combine_queued_prompts_enabled() {
        agent.session.dequeue_combined_prompt(editing_id)
    } else {
        agent.session.dequeue_prompt()
    } {
        Some(q) => q,
        None => return QueueDrain::blocked(),
    };

    // A new turn is starting: follow-up chips belong to the previous response and must not linger into it
    agent.clear_follow_ups();

    // This client is now sending its own prompt: it "takes the wheel" and is no longer a passive viewer
    // Clearing this restores the strict prompt-id gate, so stale chunks from a later rewind/cancel of this turn are dropped, not adopted
    // See `AgentView::attached_as_viewer`
    agent.attached_as_viewer = false;

    ulog::info(
        "prompt.drain",
        Some(&session_id.0),
        Some(serde_json::json!({
            "kind": queued.kind.as_label(),
            "remaining_in_queue": agent.session.pending_prompts.len(),
            "prompt_len": queued.text.len(),
        })),
    );
    // qtrace: a local drip-feed drain promotes this prompt to the running turn client-side (renders a scrollback block and sets current_prompt_id)
    // In leader mode this is the suspected divergence point: the server may queue the prompt behind others instead of running it
    let logged_text = if queued.kind == QueueEntryKind::Command {
        queued
            .text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    } else {
        queued.text.chars().take(48).collect()
    };
    tracing::debug!(
        target: "qtrace",
        pid = std::process::id(),
        event = "local_drain",
        kind = queued.kind.as_label(),
        remaining = agent.session.pending_prompts.len(),
        shared_queue_len = agent.shared_queue.len(),
        session = session_id.0.as_ref(),
        text = %logged_text,
        "draining prompt LOCALLY as a new running turn",
    );

    let agent_id = agent.session.id;

    // Track whether this turn is a bash-mode command for post-turn focus.
    agent.bash_turn = queued.kind == QueueEntryKind::BashCommand;
    // Generate a fresh prompt_id for every outgoing prompt/command
    // It is threaded through PromptRequest._meta to the agent and echoed on every SessionNotification and the PromptResponse
    // That lets us correlate notifications back to the originating prompt for cancel/rewind
    let prompt_id = uuid::Uuid::new_v4().to_string();

    // Record it as self-originated so the ACP gate treats this turn's deltas as ours rather than adopting them as another client's turn
    // Ours means drive it, and drop a stale post-rewind chunk on a mismatch
    agent.note_self_originated_prompt(&prompt_id);

    match queued.kind {
        QueueEntryKind::Prompt => {
            agent.begin_local_turn(&prompt_id);
            // Scrollback shows display text (never raw skill XML)
            // Combined drains paint one bubble per original follow-up
            let is_skill = queued.display_as_skill;
            let multi = distill_prompt_queue::is_combined(&queued.combined_texts);
            let (prompt_idx, prompt_entry_id, combined_entries) = if multi {
                let (first_idx, _, last_id, all_ids) =
                    paint_or_reuse_combined_user_bubbles(agent, &queued.combined_texts);
                (first_idx, last_id, all_ids)
            } else {
                let block = if is_skill {
                    RenderBlock::skill_prompt(&queued.text)
                } else if !queued.skill_token_ranges.is_empty() {
                    RenderBlock::user_prompt_with_skill_tokens(
                        &queued.text,
                        queued.skill_token_ranges.clone(),
                    )
                } else {
                    RenderBlock::user_prompt(&queued.text)
                };
                let id = agent.scrollback.push_block(block);
                (agent.scrollback.len().saturating_sub(1), id, vec![id])
            };
            // Stash for cancel-with-restore
            // Only plain (non-skill) prompts can be reversed back into the input box
            if queued.wire_blocks.is_none() {
                let earlier = combined_entries
                    .iter()
                    .copied()
                    .filter(|id| *id != prompt_entry_id)
                    .collect();
                agent.session.in_flight_prompt = Some(crate::app::agent::InFlightPrompt {
                    text: queued.text.clone(),
                    images: queued.images.clone(),
                    scrollback_entry: prompt_entry_id,
                    combined_scrollback_entries: earlier,
                    chip_elements: queued.chip_elements.clone(),
                });
            }
            let flip = page_flip_on_send();
            agent.scrollback.follow_new_turn(Some(prompt_idx), flip);

            let combined_segs = queued.combined_texts.clone();
            let effects = if let Some(mut blocks) = queued.wire_blocks {
                // Skill injection: send structured blocks.
                // Annotate the first text block's meta with the display text
                // Without the annotation, replay shows the raw skill instructions instead of the user-facing display text
                if let Some(acp::ContentBlock::Text(tb)) = blocks.first_mut() {
                    let map = tb.meta.get_or_insert_with(acp::Meta::new);
                    map.insert(
                        user_prompt_meta::DISPLAY_TEXT.into(),
                        serde_json::Value::String(queued.text),
                    );
                    if is_skill {
                        map.insert(
                            user_prompt_meta::DISPLAY_AS_SKILL.into(),
                            serde_json::Value::Bool(true),
                        );
                    }
                    distill_prompt_queue::stamp_combined_display_texts(map, &combined_segs);
                } else {
                    tracing::debug!(
                        "wire_blocks[0] is not TextContent — displayText annotation skipped"
                    );
                }
                vec![Effect::SendPromptBlocks {
                    agent_id,
                    session_id,
                    blocks,
                    prompt_id,
                }]
            } else if !queued.images.is_empty() {
                // Image-bearing prompt: build text and image content blocks
                // Pass the session cwd so orphan `[Image #N: <path>]` placeholders can be recovered from disk via the shared helper
                // Token ranges are not stamped here: the builder rewrites the text (placeholder stripping), which would shift byte offsets
                let mut blocks = crate::prompt_images::build_content_blocks_with_workspace(
                    queued.text,
                    queued.images,
                    Some(std::path::Path::new(&agent.session.cwd)),
                );
                if let Some(acp::ContentBlock::Text(tb)) = blocks.first_mut() {
                    let map = tb.meta.get_or_insert_with(acp::Meta::new);
                    distill_prompt_queue::stamp_combined_display_texts(map, &combined_segs);
                }
                vec![Effect::SendPromptBlocks {
                    agent_id,
                    session_id,
                    blocks,
                    prompt_id,
                }]
            } else if multi {
                // Stamp combinedDisplayTexts so reload paints multi-bubble
                // No skillTokenRanges: dequeue_combined_prompt clears them on every combined drain (multi paints plain per-segment bubbles)
                let mut tb = acp::TextContent::new(queued.text);
                let map = tb.meta.get_or_insert_with(acp::Meta::new);
                distill_prompt_queue::stamp_combined_display_texts(map, &combined_segs);
                vec![Effect::SendPromptBlocks {
                    agent_id,
                    session_id,
                    blocks: vec![acp::ContentBlock::Text(tb)],
                    prompt_id,
                }]
            } else {
                // Normal prompt: send text as-is.
                vec![Effect::SendPrompt {
                    agent_id,
                    session_id,
                    text: queued.text,
                    prompt_id,
                    skill_token_ranges: queued.skill_token_ranges,
                }]
            };
            QueueDrain {
                effects,
                page_flip_entry: flip.then_some(prompt_entry_id),
            }
        }
        QueueEntryKind::Command => {
            let invocation = crate::slash::parse_invocation(&queued.text);
            let token = invocation.as_ref().map_or("", |inv| inv.token);
            let (command, started, effect) = match token {
                "flush" => (
                    AgentCommand::MemoryFlush,
                    SessionEvent::MemoryCommandStarted {
                        command: MemoryCommandKind::Flush,
                    },
                    Effect::MemoryFlush {
                        agent_id,
                        session_id,
                    },
                ),
                "dream" => (
                    AgentCommand::MemoryDream,
                    SessionEvent::MemoryCommandStarted {
                        command: MemoryCommandKind::Dream,
                    },
                    Effect::MemoryDream {
                        agent_id,
                        session_id,
                    },
                ),
                _ => {
                    let user_context = invocation
                        .map(|inv| inv.args.trim())
                        .filter(|args| !args.is_empty())
                        .map(str::to_string);
                    (
                        AgentCommand::Compact,
                        SessionEvent::CompactStarted,
                        Effect::Compact {
                            agent_id,
                            session_id,
                            user_context,
                        },
                    )
                }
            };
            agent.session.start_command(command);
            // The command owns the pane; a leftover wake marker must not shadow stop.
            agent.running_wake_turn = None;
            agent.turn_started_at = Some(Instant::now());
            // Invocation marker: each run visibly owns its outcome line (completed/cancelled/failed)
            // For `/compact` this matches the auto path's "Context N% full. Compacting…".
            // Local block only: like the outcome lines it is not persisted, so resume replays neither
            agent
                .scrollback
                .push_block(RenderBlock::session_event(started));

            QueueDrain {
                effects: vec![effect],
                page_flip_entry: None,
            }
        }
        QueueEntryKind::BashCommand => {
            // Start turn but do not push a user prompt block
            // The execute block from the shell is the visual entry
            agent.start_turn_boundary(Some(&prompt_id));
            agent.session.current_prompt_id = Some(prompt_id.clone());
            agent.arm_prompt_ack(&prompt_id, Instant::now());
            agent.turn_started_at = Some(Instant::now());

            agent.scrollback.follow_new_turn(None, page_flip_on_send());

            QueueDrain {
                effects: vec![Effect::SendBashCommand {
                    agent_id,
                    session_id,
                    command: queued.text,
                    prompt_id,
                }],
                page_flip_entry: None,
            }
        }
    }
}

/// Whether [`apply_turn_start_shim`] renders its own user block (i.e. `display_block` is `Some`).
/// When true the pager owns the block and must swallow the leader's user-echo.
/// When false (bash, or a viewer with no local text) the echo is the only source and must render.
pub(crate) fn shim_renders_own_user_block(kind: &str, text: Option<&str>) -> bool {
    match kind {
        "bash" => false,
        _ => text.is_some(),
    }
}

/// Trailing turn-starting `UserPrompt` matching `text`, scanning back past turn-boundary chrome.
/// `claim_interjection` skips other interjections and keeps the oldest match.
fn trailing_user_prompt_matching(
    agent: &AgentView,
    text: &str,
    claim_interjection: bool,
) -> Option<(usize, crate::scrollback::EntryId)> {
    let mut found = None;
    for idx in (0..agent.scrollback.len()).rev() {
        let Some(entry) = agent.scrollback.entry(idx) else {
            break;
        };
        match &entry.block {
            RenderBlock::UserPrompt(ub) if ub.text == text && ub.is_interjection => {
                if claim_interjection {
                    found = Some((idx, entry.id));
                    continue;
                }
                break;
            }
            RenderBlock::UserPrompt(ub) if ub.text == text && !ub.is_interjection => {
                if !claim_interjection {
                    return Some((idx, entry.id));
                }
                break;
            }
            RenderBlock::UserPrompt(ub) if claim_interjection && ub.is_interjection => continue,
            RenderBlock::SessionEvent(_) | RenderBlock::System(_) => continue,
            _ => break,
        }
    }
    found
}

/// Last `n` non-interjection user prompts (oldest to newest), scanning back past turn chrome.
/// Used to reuse multi-bubble paints when the echo already rendered.
fn trailing_user_prompts(
    agent: &AgentView,
    n: usize,
) -> Option<Vec<(usize, crate::scrollback::EntryId, String)>> {
    if n == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for idx in (0..agent.scrollback.len()).rev() {
        let entry = agent.scrollback.entry(idx)?;
        match &entry.block {
            RenderBlock::UserPrompt(ub) if !ub.is_interjection => {
                out.push((idx, entry.id, ub.text.clone()));
                if out.len() == n {
                    out.reverse();
                    return Some(out);
                }
            }
            RenderBlock::SessionEvent(_) | RenderBlock::System(_) => continue,
            _ => return None,
        }
    }
    None
}

/// Paint one user bubble per combined segment, or reuse matching trailing bubbles.
/// Drops a single joined-body bubble if the echo raced ahead.
/// Returns `(first_idx, first_id, last_id, all_segment_ids)`, with segment ids oldest to newest.
fn paint_or_reuse_combined_user_bubbles(
    agent: &mut AgentView,
    segments: &[String],
) -> (
    usize,
    crate::scrollback::EntryId,
    crate::scrollback::EntryId,
    Vec<crate::scrollback::EntryId>,
) {
    if let Some(existing) = trailing_user_prompts(agent, segments.len()).filter(|rows| {
        rows.iter()
            .map(|(_, _, t)| t.as_str())
            .eq(segments.iter().map(String::as_str))
    }) && let (Some((first_idx, first_id, _)), Some((_, last_id, _))) =
        (existing.first(), existing.last())
    {
        let ids = existing.iter().map(|(_, id, _)| *id).collect();
        return (*first_idx, *first_id, *last_id, ids);
    }

    let joined = distill_prompt_queue::join_texts(segments.iter().map(String::as_str));
    if let Some((_, id)) = trailing_user_prompt_matching(agent, &joined, false) {
        agent.scrollback.remove_entry(id);
    }

    let mut first_idx = None;
    let mut first_id = None;
    let mut last_id = None;
    let mut all_ids = Vec::with_capacity(segments.len());
    for seg in segments {
        let id = agent
            .scrollback
            .push_block(RenderBlock::user_prompt(seg.clone()));
        all_ids.push(id);
        if first_idx.is_none() {
            first_idx = Some(agent.scrollback.len().saturating_sub(1));
            first_id = Some(id);
        }
        last_id = Some(id);
    }
    (
        first_idx.expect("segments non-empty"),
        first_id.expect("segments non-empty"),
        last_id.expect("segments non-empty"),
        all_ids,
    )
}

/// Paint the user block for a send-now'd prompt at dispatch.
/// Without this paint the message has no visible representation until the turn-start adoption (which reuses the block via `send_now_painted_blocks`).
/// Call wherever the expectation is armed, only for rows whose adoption paints its own block; `kind` must build the same block the shim would.
pub(super) fn push_send_now_user_block(
    agent: &mut AgentView,
    prompt_id: &str,
    kind: &str,
    text: &str,
    edited: bool,
) {
    // Viewers never run the shim and keep their echo (their only block source); painting here would double-render against it
    if agent.attached_as_viewer {
        return;
    }
    // Repeat arm: same text is already painted; new text (edit re-arm) replaces the block
    if let Some((existing, _)) = agent.send_now_painted_blocks.get(prompt_id).copied()
        && let Some(idx) = agent.scrollback.index_of_id(existing)
    {
        let same = matches!(
            agent.scrollback.entry(idx).map(|e| &e.block),
            Some(RenderBlock::UserPrompt(ub)) if ub.text == text
        );
        if same {
            return;
        }
        agent.scrollback.remove_entry(existing);
    }
    // Always a fresh block: at dispatch this prompt's echo cannot have landed yet, so a same-text trailing match is a stale earlier bubble
    // Claiming it would leave the send-now with no new visible message
    let block = match kind {
        "cron" => RenderBlock::cron_prompt(text.to_string()),
        _ => RenderBlock::user_prompt(text.to_string()),
    };
    let entry_id = agent.scrollback.push_block(block);
    // Send-now keeps the viewport where it is (no entry-top jump).
    agent.scrollback.enable_follow_mode();
    agent
        .send_now_painted_blocks
        .insert(prompt_id.to_string(), (entry_id, edited));
    agent
        .send_now_echo_pending
        .insert(prompt_id.to_string(), text.trim().to_string());
}

/// Returns `false` unless the client expects a Send Now cancel, or the row belongs to an active goal on a committed, running turn.
/// Arms the cancel expectation only on the expects-cancel path.
/// An active goal paints without arming so its interjection notification can claim the block.
fn paint_send_now_and_maybe_arm(agent: &mut AgentView, id: &str) -> bool {
    let expects_cancel = agent.expects_send_now_cancel();
    let goal_active = agent
        .goal_state
        .as_ref()
        .is_some_and(|goal| matches!(goal.status, crate::app::agent::GoalDisplayStatus::Active));
    if !(expects_cancel
        || (goal_active && agent.session.state.is_turn_running() && agent.front_message_committed))
    {
        return false;
    }
    if expects_cancel {
        agent.arm_send_now_expectation(id.to_string());
    }
    true
}

/// Active goals paint without arming cancellation so their authoritative interjection notification can claim the block.
pub(super) fn arm_send_now_and_paint_dispatched(
    agent: &mut AgentView,
    prompt_id: &str,
    text: &str,
) {
    if !paint_send_now_and_maybe_arm(agent, prompt_id) {
        return;
    }
    push_send_now_user_block(agent, prompt_id, "prompt", text, /* edited */ false);
}

/// Active-goal rows paint without arming cancellation; bash rows remain queued.
pub(crate) fn arm_send_now_and_paint(agent: &mut AgentView, id: &str, new_text: Option<&str>) {
    if !paint_send_now_and_maybe_arm(agent, id) {
        return;
    }
    let row = agent
        .shared_queue
        .iter()
        .find(|e| e.id == id)
        .map(|e| (e.kind.clone(), e.text.clone()));
    if let Some((kind, text)) = row
        && shim_renders_own_user_block(&kind, Some(&text))
    {
        let edited = new_text.is_some_and(|t| t != text);
        push_send_now_user_block(agent, id, &kind, new_text.unwrap_or(&text), edited);
    }
}

/// It does not mint a `prompt_id`; it adopts the one the leader reported.
/// It does not emit a send `Effect`; the prompt was already sent at enqueue time.
/// `start_turn` calls `expect_user_echo`, so the optimistic block here and the server's user-echo `session/update` are de-duplicated.
pub(crate) fn apply_turn_start_shim(
    agent: &mut AgentView,
    prompt_id: String,
    text: Option<String>,
    kind: &str,
    combined_texts: Option<Vec<String>>,
) -> Option<EntryId> {
    // Re-derive the per-turn viewer flag (see the ACP gate)
    // If this client originated it (its own queued/immediate prompt), it drives it; otherwise it is viewing a turn another client drives
    // `attached_as_viewer` must then flip back to true even if this pane has sent prompts before (the flag is not a one-way latch)
    let adopted_from_other_client = !agent.is_self_originated_prompt(&prompt_id);
    // Sticky pin and still-armed send-now expect (not cleared on adopt; the cancel rail may still need it)
    // Either covers adopt-before-cancel
    let skip_entry_top = agent
        .follow_without_jump_prompt_id
        .as_ref()
        .is_some_and(|id| id == &prompt_id)
        || agent
            .expect_send_now_cancel
            .as_ref()
            .is_some_and(|id| id == &prompt_id);
    // Always drop pin (hit or miss) so a stale queue-row id cannot skip later.
    agent.follow_without_jump_prompt_id = None;
    tracing::debug!(
        target: "qtrace",
        pid = std::process::id(),
        event = "turn_start_shim",
        prompt_id = %prompt_id,
        kind,
        adopted_from_other_client,
        skip_entry_top,
        prev_current_prompt_id = agent.session.current_prompt_id.as_deref().unwrap_or(""),
        shared_queue_len = agent.shared_queue.len(),
        text = %text.as_deref().unwrap_or("").chars().take(48).collect::<String>(),
        "adopting server-driven running turn (turn-start shim)",
    );
    let send_now_paint = agent.send_now_painted_blocks.contains_key(&prompt_id)
        || agent.send_now_echo_pending.contains_key(&prompt_id);
    agent.start_turn_boundary(Some(&prompt_id));
    if send_now_paint {
        agent.session.tracker.clear_user_echo_skip();
    }
    agent.session.current_prompt_id = Some(prompt_id.clone());
    agent.attached_as_viewer = adopted_from_other_client;
    // A new (adopted) turn is starting: drop the prior turn's chips but keep the seen ring
    // A buffer-replayed `x.ai/follow_ups` for an older response then stays rejected (no stale revival)
    // No seen-ring un-recording is therefore needed
    agent.clear_follow_ups();
    // The adopted turn's follow_ups may have arrived on the ext channel before this turn-start adoption (separate channels) and been buffered
    // Render them now that the turn is current
    agent.flush_pending_follow_ups(&prompt_id);

    // Combined turn: one user bubble per original follow-up (painted below).
    let multi_segments: Option<Vec<String>> = combined_texts.filter(|v| v.len() >= 2);

    // Display block (if any) and whether Ctrl+C can restore into the composer
    let (display_block, rewindable): (Option<RenderBlock>, bool) = match kind {
        "bash" => {
            agent.bash_turn = true;
            (None, false)
        }
        "cron" => (text.as_deref().map(RenderBlock::cron_prompt), false),
        _ if multi_segments.is_some() => (None, true),
        _ => (text.as_deref().map(RenderBlock::user_prompt), true),
    };

    debug_assert!(
        multi_segments.is_some()
            || display_block.is_some() == shim_renders_own_user_block(kind, text.as_deref()),
        "shim_renders_own_user_block must mirror apply_turn_start_shim's display_block"
    );

    let page_flip_entry = if let Some(segments) = multi_segments {
        let (prompt_idx, first_id, last_id, all_ids) =
            paint_or_reuse_combined_user_bubbles(agent, &segments);
        if rewindable {
            let restore = text.clone().unwrap_or_else(|| {
                distill_prompt_queue::join_texts(segments.iter().map(String::as_str))
            });
            let earlier = all_ids.into_iter().filter(|id| *id != last_id).collect();
            // An adopted turn arrives with text only, never the original attachments, so a Ctrl+C rewind restores just the joined text
            // The local drain path, which owns the data, restores images/chips.
            agent.session.in_flight_prompt = Some(crate::app::agent::InFlightPrompt {
                text: restore,
                images: Vec::new(),
                scrollback_entry: last_id,
                combined_scrollback_entries: earlier,
                chip_elements: Vec::new(),
            });
        }
        if skip_entry_top {
            agent.scrollback.set_selected(Some(prompt_idx));
            agent.scrollback.enable_follow_mode();
            None
        } else {
            let flip = page_flip_on_send();
            agent.scrollback.follow_new_turn(Some(prompt_idx), flip);
            flip.then_some(first_id)
        }
    } else if let Some(block) = display_block {
        // The block may already be painted: consume the send-now paint's id-keyed entry, else reuse a trailing echo block by text
        // Never double-push the user-prompt row
        let claim_interjection = prompt_id.starts_with("interject-fallback-");
        let map_painted = agent.send_now_painted_blocks.remove(&prompt_id).and_then(
            |(id, edited)| -> Option<(usize, crate::scrollback::EntryId)> {
                let idx = agent.scrollback.index_of_id(id)?;
                // Text drift resolves by freshness: an `edited` paint is newer than the adoption's captured mirror text, so keep it
                // Otherwise the adoption is fresher: swap the stale block
                let RenderBlock::UserPrompt(ub) = &agent.scrollback.entry(idx)?.block else {
                    return None;
                };
                if text.as_deref() != Some(ub.text.as_str()) && !edited {
                    agent.scrollback.remove_entry(id);
                    match text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                        Some(fresh) => {
                            agent
                                .send_now_echo_pending
                                .insert(prompt_id.clone(), fresh.to_string());
                        }
                        None => {
                            agent.send_now_echo_pending.remove(&prompt_id);
                        }
                    }
                    return None;
                }
                // Drop an unarmed echo's duplicate copy of this prompt
                // Blocks claimed by other pending send-nows are excluded (identical stacked texts)
                if let Some((_, dup)) = text
                    .as_deref()
                    .and_then(|t| trailing_user_prompt_matching(agent, t, claim_interjection))
                    && dup != id
                    && !agent
                        .send_now_painted_blocks
                        .values()
                        .any(|(v, _)| *v == dup)
                {
                    agent.scrollback.remove_entry(dup);
                    agent.send_now_echo_pending.remove(&prompt_id);
                }
                Some((agent.scrollback.index_of_id(id)?, id))
            },
        );
        let reused_echo = map_painted.is_none();
        let already_painted = map_painted.or_else(|| {
            text.as_deref()
                .and_then(|t| trailing_user_prompt_matching(agent, t, claim_interjection))
                // Never claim a block owned by another pending send-now.
                .filter(|(_, id)| !agent.send_now_painted_blocks.values().any(|(v, _)| v == id))
        });
        if reused_echo && already_painted.is_some() {
            agent.send_now_echo_pending.remove(&prompt_id);
        }
        let (prompt_idx, prompt_entry_id) = if let Some(found) = already_painted {
            if claim_interjection
                && let Some(RenderBlock::UserPrompt(ub)) =
                    agent.scrollback.entry_mut(found.0).map(|e| &mut e.block)
            {
                ub.is_interjection = false;
            }
            found
        } else {
            let id = agent.scrollback.push_block(block);
            (agent.scrollback.len().saturating_sub(1), id)
        };
        if rewindable && let Some(text) = text {
            // The rewind restore must match the on-screen (possibly edited) block text, not the adoption's stale mirror text
            let restore_text = match agent.scrollback.entry(prompt_idx).map(|e| &e.block) {
                Some(RenderBlock::UserPrompt(ub)) if ub.text != text => ub.text.clone(),
                _ => text,
            };
            agent.session.in_flight_prompt = Some(crate::app::agent::InFlightPrompt {
                text: restore_text,
                images: Vec::new(),
                scrollback_entry: prompt_entry_id,
                combined_scrollback_entries: Vec::new(),
                chip_elements: Vec::new(),
            });
        }
        if skip_entry_top {
            // Send-now: follow at the tail; never entry-top jump.
            agent.scrollback.set_selected(Some(prompt_idx));
            agent.scrollback.enable_follow_mode();
            None
        } else {
            let flip = page_flip_on_send();
            agent.scrollback.follow_new_turn(Some(prompt_idx), flip);
            flip.then_some(prompt_entry_id)
        }
    } else {
        // `start_turn` above called `expect_user_echo`, which would swallow the agent's live user-message broadcast
        // For these turns that broadcast is the only source of the user block
        // Clear the skip so `handle_user_message` renders it instead of dropping it (the cause of viewers missing the cron header)
        agent.session.tracker.clear_user_echo_skip();
        agent.scrollback.follow_new_turn(None, page_flip_on_send());
        None
    };

    agent.turn_started_at = Some(Instant::now());

    // After the echo-skip above; mirror the live-arm transitions the flush bypasses.
    agent.flush_pending_adoption_updates(&prompt_id);
    if agent.session.tracker.activity().is_some() {
        agent.session.in_flight_prompt = None;
    }
    if let Some(commands) = agent.session.tracker.take_pending_acp_commands() {
        agent
            .session
            .replace_available_commands(commands, CommandCatalogSource::QueueDrain);
    }
    if let Some(tools) = agent.session.tracker.take_pending_acp_tools() {
        agent.session.available_tools = Some(tools.into_iter().collect());
    }
    page_flip_entry
}

pub(crate) fn note_peek_page_flip(
    app: &mut AppView,
    agent_id: AgentId,
    page_flip_entry: Option<EntryId>,
) {
    let Some(entry_id) = page_flip_entry else {
        return;
    };
    let Some(mut dash) = app.dashboard.take() else {
        return;
    };
    dash.note_page_flip_for_lease(agent_id, entry_id, &app.agents);
    app.dashboard = Some(dash);
}

/// Drain the next queued prompt and, when that page-flips under a lease, note it.
pub(crate) fn maybe_drain_queue_and_note_peek(app: &mut AppView, agent_id: AgentId) -> Vec<Effect> {
    let drain = {
        let Some(agent) = app.agents.get_mut(&agent_id) else {
            return vec![];
        };
        maybe_drain_queue(agent)
    };
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    drain.effects
}

/// Try to drain the next queued prompt (triggered after editing completes).
pub(super) fn dispatch_drain_queue(app: &mut AppView) -> Vec<Effect> {
    if app.reconnect_pending {
        return vec![];
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    maybe_drain_queue_and_note_peek(app, id)
}

/// `Action::QueueInterjectShared` arm: map the (possibly edited) queue interject to a fire-and-forget effect scoped to the active agent's session.
pub(super) fn dispatch_queue_interject_shared(
    app: &mut AppView,
    id: String,
    expected_version: u64,
    new_text: Option<String>,
) -> Vec<Effect> {
    match active_agent_session_id(app) {
        Some(session_id) => {
            with_active_agent(app, |agent| {
                // Edited override is user-typed text; keep it Ctrl+R recallable.
                if let Some(text) = &new_text {
                    agent.record_prompt_in_history(text);
                }
                agent.release_hook_block_hold();
                agent.note_self_originated_prompt(&id);
                arm_send_now_and_paint(agent, &id, new_text.as_deref());
            });
            vec![Effect::QueueInterject {
                session_id,
                id,
                expected_version,
                new_text,
            }]
        }
        None => vec![],
    }
}

/// Decides whether the edited row is dropped and whether its text reaches the send path.
enum EditedCommandGate {
    /// Drop the row and run the command. Carries the bound session a server-row removal needs.
    Run { session_id: acp::SessionId },
    /// `dispatch_send_prompt_inner` refuses this one before running it: let it print the refusal and keep the row.
    RefusedBySendPath,
    /// No bound session: a server-row removal has no address, and a command that needs one would fail after the row was gone.
    /// Keep the row and say so.
    NeedsSession,
}

fn preserve_queued_image_paths(
    app: &AppView,
    submission: &mut crate::views::prompt_widget::StashedPrompt,
) {
    let retained = app
        .agents
        .values()
        .flat_map(|agent| agent.session.pending_prompts.iter())
        .flat_map(|prompt| prompt.images.iter())
        .map(|image| image.preview.identity())
        .collect::<std::collections::HashSet<_>>();
    submission.disarm_image_cleanup(&retained);
}

/// `Action::RunEditedQueuedCommand` arm: the row's edited text resolved to a pager builtin.
/// Drop the row and run the text through slash dispatch, the owner of resolution and command telemetry.
/// A refusal thus leaves the row queued with its pre-edit text instead of trading it for a hint.
pub(super) fn dispatch_run_edited_queued_command(
    app: &mut AppView,
    local_id: u64,
    server: Option<crate::app::actions::SharedQueueTarget>,
    mut submission: crate::views::prompt_widget::StashedPrompt,
) -> Vec<Effect> {
    if app.reconnect_pending {
        // Nothing runs and nothing drains while reconnecting (see `dispatch_drain_queue`), so the row just stays put
        app.show_toast(super::prompt::RECONNECTING_NOTICE);
        preserve_queued_image_paths(app, &mut submission);
        return vec![];
    }
    // The send half is bound to the active view, so resolve the removal target the same way
    // (The dashboard popup forwards keys to an attached agent without switching it.)
    // The edit exit has already taken the composer text, so a silent bail would drop the command without a trace
    let ActiveView::Agent(agent_id) = app.active_view else {
        app.show_toast("Open the session to run this command");
        preserve_queued_image_paths(app, &mut submission);
        return vec![];
    };
    // Resolve every command refusal before removing the row. The send path will surface the same
    // refusal after this gate keeps the original queued prompt and its attachments intact.
    let screen_mode = app.screen_mode;
    let gate = {
        let Some(agent) = app.agents.get(&agent_id) else {
            preserve_queued_image_paths(app, &mut submission);
            return vec![];
        };
        let registry = agent.prompt.slash_controller.registry();
        let command_refused =
            crate::slash::parse_invocation(submission.text.trim()).is_some_and(|invocation| {
                registry
                    .get_for_dispatch(invocation.token)
                    .is_some_and(|command| {
                        command
                            .mode_support()
                            .refusal(invocation.token, screen_mode)
                            .is_some()
                            || command
                                .submission_refusal(
                                    invocation.args,
                                    /* voice_owns_prompt */ false,
                                )
                                .is_some()
                    })
            });
        if command_refused {
            EditedCommandGate::RefusedBySendPath
        } else {
            // Fail closed on the session itself rather than on what a command declares
            // `session_scoped` is a menu-offering hint, so it says nothing reliable about whether `run()` needs a bound session
            match agent.session.session_id.clone() {
                Some(session_id) => EditedCommandGate::Run { session_id },
                None => EditedCommandGate::NeedsSession,
            }
        }
    };

    let mut effects = Vec::new();
    let sends = match gate {
        EditedCommandGate::Run { session_id } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            match server {
                // Server rows are never mutated client-side; the `x.ai/queue/changed` rebroadcast is the visual result
                Some(server) => effects.push(Effect::QueueRemove {
                    session_id,
                    id: server.id,
                    expected_version: server.expected_version,
                }),
                None => {
                    if let Some(removed) = agent.remove_local_queue_row(local_id) {
                        let retained: std::collections::HashSet<u64> = submission
                            .images
                            .iter()
                            .map(|image| image.preview.identity())
                            .collect();
                        for image in &removed.images {
                            if !retained.contains(&image.preview.identity()) {
                                crate::prompt_images::cleanup_image(
                                    crate::prompt_images::SessionPathPolicy::Preserve,
                                    image,
                                );
                            }
                        }
                    }
                }
            }
            true
        }
        EditedCommandGate::RefusedBySendPath => {
            preserve_queued_image_paths(app, &mut submission);
            true
        }
        // Row kept and nothing runs: a command that ignores the missing session (`/compact` enqueues regardless) would leave a second row
        EditedCommandGate::NeedsSession => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.show_toast(NO_SESSION_NOTICE);
            }
            preserve_queued_image_paths(app, &mut submission);
            false
        }
    };
    if sends {
        let text = submission.text.clone();
        effects.extend(super::prompt::dispatch_send_prompt_submission(
            app,
            text,
            Some(submission),
            /* consume_input */ false,
            /* literal */ false,
            /* is_follow_up */ false,
        ));
    }
    // The edit lock is released either way, so a command that starts no turn (or a refusal that keeps the row) must not strand the queue
    // This matches the plain save's `DrainQueue`
    effects.extend(maybe_drain_queue_and_note_peek(app, agent_id));
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::actions::{Action, SharedQueueTarget};
    use crate::app::agent::AgentState;
    use crate::app::agent_view::test_fixtures::{
        complete_task_output_wait_call, count_turn_markers, running_subagent_info,
        simulate_subagent_wait, simulate_task_output_wait, simulate_task_output_wait_call,
    };
    use crate::app::dispatch::router::dispatch;
    use crate::app::dispatch::tests::{
        end_turn, enqueue_local, last_system_text, test_agent, test_app_with_agent,
    };

    /// A running background bash task for the work-count fixtures.
    fn running_bg_task(task_id: &str) -> crate::app::agent::BgTaskState {
        crate::app::agent::BgTaskState {
            task_id: task_id.into(),
            tool_call_id: format!("call-{task_id}"),
            command: "sleep 5".into(),
            description: None,
            cwd: "/tmp".into(),
            output_file: "/tmp/out".into(),
            status: crate::app::agent::BgTaskStatus::Running,
            start_time: std::time::SystemTime::now(),
            end_time: None,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_line_count: 0,
            truncated: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            is_monitor: false,
            restored_from_replay: false,
        }
    }

    #[test]
    fn dream_drain_runs_as_a_tracked_command_with_an_outcome_line() {
        use crate::app::actions::TaskResult;
        use crate::app::dispatch::task_result::dispatch_task_result;
        use crate::scrollback::blocks::SessionEvent;
        use distill_shell::extensions::memory::{MemoryDreamDisposition, MemoryDreamResponse};

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.enqueue_command("/dream".into());
        let drain = maybe_drain_queue(agent);
        assert!(matches!(
            drain.effects.as_slice(),
            [Effect::MemoryDream { .. }]
        ));
        assert_eq!(
            agent.session.state.command_in_flight(),
            Some(&AgentCommand::MemoryDream)
        );

        dispatch_task_result(
            TaskResult::MemoryDreamComplete {
                agent_id: id,
                result: Ok(MemoryDreamResponse {
                    disposition: MemoryDreamDisposition::Completed,
                    observation_count: 7,
                    topics_affected: 3,
                }),
            },
            &mut app,
        );

        let agent = test_agent(&app, id);
        assert!(agent.session.state.is_idle());
        let events: Vec<SessionEvent> = (0..agent.scrollback.len())
            .filter_map(|i| match agent.scrollback.entry(i).map(|e| &e.block) {
                Some(RenderBlock::SessionEvent(ev)) => Some(ev.event.clone()),
                _ => None,
            })
            .collect();
        match events.as_slice() {
            [
                SessionEvent::MemoryCommandStarted { .. },
                SessionEvent::MemoryCommandCompleted {
                    summary,
                    succeeded: true,
                    ..
                },
            ] => assert_eq!(summary, "Dream merged 7 observations into 3 topics."),
            other => panic!("expected marker → outcome, got {other:?}"),
        }
    }

    #[test]
    fn compact_drain_pushes_one_marker_paired_with_its_outcome() {
        use crate::app::actions::TaskResult;
        use crate::app::dispatch::task_result::dispatch_task_result;
        use crate::scrollback::blocks::SessionEvent;
        use distill_shell::session::helpers::session_compact::{
            CompactErrorKind, compact_error_data,
        };

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let wire_error = agent_client_protocol::Error::internal_error().data(compact_error_data(
            CompactErrorKind::Failed,
            "API error (status 500 Internal Server Error): should never be seen",
        ));

        // Two back-to-back invocations: each drain pushes exactly one marker, so each failure line is visibly owned by its own `/compact`
        for _ in 0..2 {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.enqueue_command("/compact".into());
            let drain = maybe_drain_queue(agent);
            assert!(
                matches!(drain.effects.as_slice(), [Effect::Compact { .. }]),
                "command drain must start one compact, got {:?}",
                drain.effects
            );
            dispatch_task_result(
                TaskResult::CompactComplete {
                    agent_id: id,
                    result: Err(crate::app::effects::compact_error(&wire_error)),
                },
                &mut app,
            );
        }

        let agent = test_agent(&app, id);
        let events: Vec<SessionEvent> = (0..agent.scrollback.len())
            .filter_map(|i| match agent.scrollback.entry(i).map(|e| &e.block) {
                Some(RenderBlock::SessionEvent(ev)) => Some(ev.event.clone()),
                _ => None,
            })
            .collect();
        assert!(
            matches!(
                events.as_slice(),
                [
                    SessionEvent::CompactStarted,
                    SessionEvent::CompactionFailed { .. },
                    SessionEvent::CompactStarted,
                    SessionEvent::CompactionFailed { .. },
                ]
            ),
            "each invocation must render marker → outcome, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|ev| matches!(ev, SessionEvent::CompactionStarted { .. })),
            "the manual marker must not double as the auto marker"
        );
    }

    #[test]
    fn drain_blocked_when_editing_front_prompt() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // Queue 3 prompts, first one drains immediately (turn starts); the follow-ups populate the local queue directly (see `enqueue_local`)
        dispatch(Action::SendPrompt("first".into()), &mut app);
        enqueue_local(&mut app, id, "second");
        enqueue_local(&mut app, id, "third");
        assert!(test_agent(&app, id).session.state.is_turn_running());
        assert_eq!(test_agent(&app, id).session.queue_len(), 2);

        // Simulate user editing "second" (which becomes front after "first" ends).
        let Some(second_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };
        app.agents.get_mut(&id).unwrap().prompt_mode = PromptMode::EditingQueued {
            id: second_id,
            original: "second".into(),
            server_id: None,
            kind: crate::app::agent::QueueEntryKind::Prompt,
        };

        // Turn ends: must not drain "second" (user is editing it), only FetchBilling
        let effects = dispatch(end_turn(), &mut app);
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects.first(),
            Some(Effect::FetchBilling { silent: true, .. })
        ));
        assert!(test_agent(&app, id).session.state.is_idle());
        assert_eq!(test_agent(&app, id).session.queue_len(), 2);
        assert_eq!(
            test_agent(&app, id)
                .session
                .pending_prompts
                .front()
                .map(|p| p.text.as_str()),
            Some("second")
        );
    }

    #[test]
    fn drain_not_blocked_when_editing_non_front_prompt() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // Queue 3 prompts, first drains.
        dispatch(Action::SendPrompt("first".into()), &mut app);
        enqueue_local(&mut app, id, "second");
        enqueue_local(&mut app, id, "third");

        // Simulate user editing "third" (not the front)
        let Some(third_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .get(1)
            .map(|p| p.id)
        else {
            panic!("expected second queued prompt");
        };
        app.agents.get_mut(&id).unwrap().prompt_mode = PromptMode::EditingQueued {
            id: third_id,
            original: "third".into(),
            server_id: None,
            kind: crate::app::agent::QueueEntryKind::Prompt,
        };

        // Turn ends: drains "second" (front, not being edited) plus FetchBilling
        let effects = dispatch(end_turn(), &mut app);
        assert_eq!(effects.len(), 2);
        assert!(
            matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "second")
        );
        assert!(matches!(
            effects.get(1),
            Some(Effect::FetchBilling { silent: true, .. })
        ));
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);
        assert_eq!(
            test_agent(&app, id)
                .session
                .pending_prompts
                .front()
                .map(|p| p.text.as_str()),
            Some("third")
        );
    }

    // Edited row that resolved to a pager builtin

    fn run_edited_queued_submission(
        app: &mut AppView,
        local_id: u64,
        server: Option<SharedQueueTarget>,
        submission: crate::views::prompt_widget::StashedPrompt,
    ) -> Vec<Effect> {
        dispatch(
            Action::RunEditedQueuedCommand {
                local_id,
                server,
                submission,
            },
            app,
        )
    }

    fn run_edited_queued_command(
        app: &mut AppView,
        local_id: u64,
        server: Option<SharedQueueTarget>,
        text: &str,
    ) -> Vec<Effect> {
        run_edited_queued_submission(
            app,
            local_id,
            server,
            crate::views::prompt_widget::StashedPrompt::from_submission(
                text.into(),
                Vec::new(),
                Vec::new(),
            ),
        )
    }

    /// The fixture's shared queue with one row, `p1` at version 2.
    fn seed_shared_row(app: &mut AppView, id: AgentId) {
        app.agents.get_mut(&id).unwrap().shared_queue =
            vec![crate::app::prompt_queue::QueueEntryWire {
                id: "p1".into(),
                version: 2,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "what is the default".into(),
                position: 0,
                combined_texts: None,
            }];
    }

    fn shared_target() -> Option<SharedQueueTarget> {
        Some(SharedQueueTarget {
            id: "p1".into(),
            expected_version: 2,
        })
    }

    #[test]
    fn run_edited_queued_command_drops_local_row_then_runs_command() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "what is the default");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let effects =
            run_edited_queued_command(&mut app, local_id, None, "/btw what is the default");

        assert!(
            matches!(effects.as_slice(), [Effect::SendBtw { .. }]),
            "expected the command to run"
        );
        assert_eq!(test_agent(&app, id).session.queue_len(), 0);
    }

    /// Minimal hosts the feedback form, so a queued row edited into bare `/feedback` runs like any other
    /// command: the row goes, the form opens, and the drafts list is requested.
    #[test]
    fn edited_queued_bare_feedback_opens_the_modal() {
        use crate::views::feedback_modal::FeedbackDraftRequest;

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.screen_mode = crate::app::ScreenMode::Minimal;
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "original queued prompt");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let effects = run_edited_queued_command(&mut app, local_id, None, "/feedback");

        assert!(
            matches!(
                effects.as_slice(),
                [Effect::FeedbackDraftRequest {
                    request: FeedbackDraftRequest::List { .. },
                    ..
                }]
            ),
            "opening the form lists drafts: {effects:?}"
        );
        assert!(test_agent(&app, id).feedback_modal.is_some());
        assert_eq!(test_agent(&app, id).session.queue_len(), 0);
    }

    #[test]
    fn run_edited_feedback_uses_row_text_and_attachments_not_the_restored_draft() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        app.agents
            .get_mut(&id)
            .unwrap()
            .prompt
            .set_text("/feedback live draft");
        enqueue_local(&mut app, id, "queued feedback");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let image = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
            data: vec![
                0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
            mime_type: "image/png".to_owned(),
        });
        let placeholder = crate::prompt_images::display_text(1);
        let submission = crate::views::prompt_widget::StashedPrompt::from_submission(
            format!("/feedback row report {placeholder}"),
            vec![image],
            vec![crate::app::agent::ChipElement {
                range: 21..21 + placeholder.len(),
                kind: crate::views::prompt_widget::KIND_IMAGE,
                display: None,
            }],
        );
        let effects = run_edited_queued_submission(&mut app, local_id, None, submission);

        // Inline `/feedback <text>` POSTs right away, even mid-turn, with the row's own attachment.
        let [
            Effect::SendFeedback {
                feedback_text,
                images,
                ..
            },
        ] = effects.as_slice()
        else {
            panic!("edited inline /feedback must POST once, got {effects:?}");
        };
        assert_eq!("row report", feedback_text);
        assert_eq!(1, images.len(), "the row attachment rides the POST");
        assert_eq!("/feedback live draft", test_agent(&app, id).prompt.text());
        assert!(
            test_agent(&app, id).session.pending_prompts.is_empty(),
            "the edited row is gone and nothing is queued in its place"
        );
    }

    /// Server row: a versioned remove, then the command.
    /// The shared mirror is never mutated client-side: the rebroadcast is the source of truth.
    #[test]
    fn run_edited_queued_command_removes_server_row_then_runs_command() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        seed_shared_row(&mut app, id);

        let effects = run_edited_queued_command(&mut app, 7, shared_target(), "/btw why");

        match effects.as_slice() {
            [
                Effect::QueueRemove {
                    id: removed,
                    expected_version,
                    ..
                },
                Effect::SendBtw { .. },
            ] => {
                assert_eq!(removed, "p1");
                assert_eq!(*expected_version, 2);
            }
            other => panic!("expected [QueueRemove, SendBtw], got {other:?}"),
        }
        assert_eq!(test_agent(&app, id).shared_queue.len(), 1);
    }

    /// A command that returns without starting a turn must not strand the rows queued behind the one it replaced.
    #[test]
    fn run_edited_queued_command_drains_the_row_behind_it() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        enqueue_local(&mut app, id, "front");
        enqueue_local(&mut app, id, "behind");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let effects = run_edited_queued_command(&mut app, local_id, None, "/btw why");

        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SendBtw { .. }, Effect::SendPrompt { text, .. }] if text == "behind"
            ),
            "expected the command then the next row's turn"
        );
        assert_eq!(test_agent(&app, id).session.queue_len(), 0);
    }

    /// An enqueueing builtin re-enters the local queue at the tail, so the row's position is not preserved.
    #[test]
    fn run_edited_queued_command_with_enqueueing_builtin_re_adds_at_the_tail() {
        use crate::app::agent::QueueEntryKind;
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "edited into a command");
        enqueue_local(&mut app, id, "behind");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let effects = run_edited_queued_command(&mut app, local_id, None, "/compact");

        assert!(effects.is_empty(), "no turn starts mid-turn");
        let rows: Vec<(&str, QueueEntryKind)> = test_agent(&app, id)
            .session
            .pending_prompts
            .iter()
            .map(|p| (p.text.as_str(), p.kind))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("behind", QueueEntryKind::Prompt),
                ("/compact", QueueEntryKind::Command),
            ]
        );
    }

    /// Reconnecting: the guard bails before the removal, so the row survives for a retry.
    #[test]
    fn run_edited_queued_command_while_reconnecting_keeps_row() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        enqueue_local(&mut app, id, "what is the default");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };
        app.reconnect_pending = true;

        let effects = run_edited_queued_command(&mut app, local_id, None, "/btw why");

        assert!(effects.is_empty());
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);
    }

    /// The dashboard popup forwards keys to an attached agent without making it the active view.
    /// The send half would no-op there, so nothing runs, the row keeps its text, and the toast lands on the view the user is actually looking at.
    #[test]
    fn run_edited_queued_command_off_active_view_keeps_row() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        enqueue_local(&mut app, id, "what is the default");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };
        app.active_view = ActiveView::AgentDashboard;
        app.dashboard = Some(crate::views::dashboard::DashboardState::default());

        let effects = run_edited_queued_command(&mut app, local_id, None, "/btw why");

        assert!(effects.is_empty());
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);
        assert_eq!(
            app.dashboard.as_ref().unwrap().error_toast.as_deref(),
            Some("Open the session to run this command")
        );
    }

    /// A command the current screen mode refuses is a pre-execution refusal: the user gets the hint and the row keeps its pre-edit text.
    #[test]
    fn run_edited_queued_command_refused_by_screen_mode_keeps_row() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        // Mid-turn so the surviving row is observable rather than drained.
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "what is the default");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        // `/fullscreen` is minimal-only and the fixture's mode is not minimal.
        let effects = run_edited_queued_command(&mut app, local_id, None, "/fullscreen");

        assert!(effects.is_empty());
        assert_eq!(
            test_agent(&app, id)
                .session
                .pending_prompts
                .front()
                .map(|p| p.text.as_str()),
            Some("what is the default"),
            "the row survives with its pre-edit text"
        );
        assert!(last_system_text(&app, id).contains("already in fullscreen"));
    }

    /// A refused edit disarms only row-owned temps; a newly pasted file is still deleted on drop.
    #[test]
    fn refused_edit_deletes_new_pasted_temps_not_row_owned() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "what is the default");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let dir = tempfile::tempdir().unwrap();
        let owned_path = dir.path().join("owned.png");
        let pasted_path = dir.path().join("pasted.png");
        std::fs::write(&owned_path, b"owned").unwrap();
        std::fs::write(&pasted_path, b"pasted").unwrap();

        let png = crate::clipboard::ImageData {
            data: vec![
                0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
            mime_type: "image/png".to_owned(),
        };
        let mut owned = crate::prompt_images::from_clipboard_data(&png);
        owned.staged_temp_path = Some(owned_path.clone());
        let mut pasted = crate::prompt_images::from_clipboard_data(&png);
        pasted.staged_temp_path = Some(pasted_path.clone());
        let Some(front) = app
            .agents
            .get_mut(&id)
            .unwrap()
            .session
            .pending_prompts
            .front_mut()
        else {
            panic!("expected queued prompt");
        };
        front.images = vec![owned.clone()];

        let submission = crate::views::prompt_widget::StashedPrompt::from_submission(
            "/fullscreen".into(),
            vec![owned, pasted],
            Vec::new(),
        );
        let _ = run_edited_queued_submission(&mut app, local_id, None, submission);

        assert!(
            owned_path.exists(),
            "row-owned temp must not be double-deleted"
        );
        assert!(
            !pasted_path.exists(),
            "edit-only pasted temp must be deleted"
        );
        assert_eq!(
            test_agent(&app, id)
                .session
                .pending_prompts
                .front()
                .map(|p| p.text.as_str()),
            Some("what is the default"),
            "refusal must keep the row"
        );
    }

    /// A refusal releases the edit lock too, so the queue must not park behind the row that was not removed.
    #[test]
    fn run_edited_queued_command_refusal_still_drains_the_queue() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        enqueue_local(&mut app, id, "what is the default");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let effects = run_edited_queued_command(&mut app, local_id, None, "/fullscreen");

        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SendPrompt { text, .. }] if text == "what is the default"
            ),
            "expected the kept row to drain"
        );
    }

    /// Fail closed while the session is binding: the row survives (a server row's removal has no address either) and the user is told why.
    #[test]
    fn run_edited_queued_command_without_session_keeps_row() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        seed_shared_row(&mut app, id);
        app.agents.get_mut(&id).unwrap().session.session_id = None;

        let effects = run_edited_queued_command(&mut app, 7, shared_target(), "/btw why");

        assert!(effects.is_empty(), "no unaddressed QueueRemove");
        assert_eq!(test_agent(&app, id).shared_queue.len(), 1);
        assert_eq!(
            test_agent(&app, id)
                .toast
                .as_ref()
                .map(|(text, _)| text.as_str()),
            Some("No active session")
        );
    }

    /// A command that produces no effect at all still costs the row: the removal is not tied to something coming back from the send path.
    #[test]
    fn run_edited_queued_command_drops_row_for_effect_free_builtin() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        enqueue_local(&mut app, id, "edited into a command");
        let Some(local_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };

        let effects = run_edited_queued_command(&mut app, local_id, None, "/help");

        assert!(effects.is_empty(), "the palette is state, not an effect");
        assert_eq!(test_agent(&app, id).session.queue_len(), 0, "row dropped");
        assert!(
            matches!(
                test_agent(&app, id).active_modal,
                Some(crate::views::modal::ActiveModal::CommandPalette { .. })
            ),
            "the command ran"
        );
    }

    /// Row already gone (a rebroadcast or a concurrent delete): nothing to remove, but the command the user typed still runs.
    #[test]
    fn run_edited_queued_command_with_unknown_local_id_still_runs() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "someone else's row");

        let effects = run_edited_queued_command(&mut app, 9999, None, "/btw why");

        assert!(
            matches!(effects.as_slice(), [Effect::SendBtw { .. }]),
            "expected the command to run"
        );
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);
    }

    #[test]
    fn drain_queue_action_sends_front_prompt() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // Queue a prompt but don't drain (set turn running first).
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "queued");
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);

        // Set idle to simulate turn end (without going through PromptResponse).
        app.agents.get_mut(&id).unwrap().session.state = AgentState::Idle;

        // DrainQueue pops and sends
        let effects = dispatch(Action::DrainQueue, &mut app);
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "queued")
        );
        assert_eq!(test_agent(&app, id).session.queue_len(), 0);
    }

    #[test]
    fn drain_scroll_honors_page_flip_setting() {
        fn app_at_bottom() -> AppView {
            let mut app = test_app_with_agent();
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            for i in 0..40 {
                agent
                    .scrollback
                    .push_block(RenderBlock::agent_message(format!("filler {i}")));
            }
            agent.scrollback.prepare_layout(80, 8);
            agent.scrollback.goto_bottom();
            app
        }

        crate::appearance::cache::set_page_flip_on_send(false);
        let mut app = app_at_bottom();
        let bottom = test_agent(&app, AgentId(0)).scrollback.scroll_offset();
        dispatch(Action::SendPrompt("go".into()), &mut app);
        let sb = &test_agent(&app, AgentId(0)).scrollback;
        assert!(sb.is_follow_mode());
        assert!(!sb.is_follow_preserve_scroll());
        assert_eq!(sb.scroll_offset(), bottom);
        assert_eq!(sb.selected(), Some(sb.len() - 1));

        let mut app = app_at_bottom();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.scrollback.scroll_up(10);
        let reading = agent.scrollback.scroll_offset();
        dispatch(Action::SendPrompt("go".into()), &mut app);
        let sb = &test_agent(&app, AgentId(0)).scrollback;
        assert!(!sb.is_follow_mode());
        assert_eq!(sb.scroll_offset(), reading);
        assert_eq!(sb.selected(), Some(sb.len() - 1));

        crate::appearance::cache::set_page_flip_on_send(true);
        let mut app = app_at_bottom();
        dispatch(Action::SendPrompt("go".into()), &mut app);
        let sb = &test_agent(&app, AgentId(0)).scrollback;
        assert!(sb.is_follow_mode());
        assert!(sb.is_follow_preserve_scroll());
        assert_eq!(sb.selected(), Some(sb.len() - 1));

        crate::appearance::cache::set_page_flip_on_send(
            distill_shell::agent::config::UiConfig::PAGE_FLIP_ON_SEND_DEFAULT,
        );
    }

    #[test]
    fn drain_queue_when_empty_does_nothing() {
        let mut app = test_app_with_agent();
        let effects = dispatch(Action::DrainQueue, &mut app);
        assert!(effects.is_empty());
    }

    /// Server-queue edit Actions map to fire-and-forget Effects scoped to the active agent's session.
    #[test]
    fn queue_edit_actions_map_to_scoped_effects() {
        let mut app = test_app_with_agent();

        let effects = dispatch(
            Action::QueueRemoveShared {
                id: "p1".into(),
                expected_version: 4,
            },
            &mut app,
        );
        match effects.as_slice() {
            [
                Effect::QueueRemove {
                    session_id,
                    id,
                    expected_version,
                },
            ] => {
                assert_eq!(session_id.0.as_ref(), "test-session");
                assert_eq!(id, "p1");
                assert_eq!(*expected_version, 4);
            }
            other => panic!("expected QueueRemove, got {other:?}"),
        }

        let effects = dispatch(
            Action::QueueReorderShared {
                ordered_ids: vec!["p2".into(), "p1".into()],
            },
            &mut app,
        );
        match effects.as_slice() {
            [
                Effect::QueueReorder {
                    session_id,
                    ordered_ids,
                },
            ] => {
                assert_eq!(session_id.0.as_ref(), "test-session");
                assert_eq!(ordered_ids, &vec!["p2".to_string(), "p1".to_string()]);
            }
            other => panic!("expected QueueReorder, got {other:?}"),
        }

        let effects = dispatch(Action::QueueClearShared, &mut app);
        match effects.as_slice() {
            [Effect::QueueClear { session_id }] => {
                assert_eq!(session_id.0.as_ref(), "test-session");
            }
            other => panic!("expected QueueClear, got {other:?}"),
        }

        let effects = dispatch(
            Action::QueueEditShared {
                id: "p1".into(),
                new_text: "new body".into(),
            },
            &mut app,
        );
        match effects.as_slice() {
            [
                Effect::QueueEdit {
                    session_id,
                    id,
                    new_text,
                },
            ] => {
                assert_eq!(session_id.0.as_ref(), "test-session");
                assert_eq!(id, "p1");
                assert_eq!(new_text, "new body");
            }
            other => panic!("expected QueueEdit, got {other:?}"),
        }

        let effects = dispatch(
            Action::QueueInterjectShared {
                id: "p1".into(),
                expected_version: 5,
                new_text: None,
            },
            &mut app,
        );
        match effects.as_slice() {
            [
                Effect::QueueInterject {
                    session_id,
                    id,
                    expected_version,
                    new_text,
                },
            ] => {
                assert_eq!(session_id.0.as_ref(), "test-session");
                assert_eq!(id, "p1");
                assert_eq!(*expected_version, 5);
                assert_eq!(*new_text, None, "plain interject carries no override");
            }
            other => panic!("expected QueueInterject, got {other:?}"),
        }
        // Plain interjects re-send an existing queue row: no history insert.
        assert!(
            test_agent(&app, AgentId(0))
                .session
                .prompt_history
                .is_empty()
        );

        // Edited interject: the same arm carrying the edited text as the newText override
        let effects = dispatch(
            Action::QueueInterjectShared {
                id: "p1".into(),
                expected_version: 5,
                new_text: Some("edited body".into()),
            },
            &mut app,
        );
        match effects.as_slice() {
            [
                Effect::QueueInterject {
                    session_id,
                    id,
                    expected_version,
                    new_text,
                },
            ] => {
                assert_eq!(session_id.0.as_ref(), "test-session");
                assert_eq!(id, "p1");
                assert_eq!(*expected_version, 5);
                assert_eq!(new_text.as_deref(), Some("edited body"));
            }
            other => panic!("expected single QueueInterject with newText, got {other:?}"),
        }
        // The user typed the edited text; it must be Ctrl+R recallable
        assert_eq!(
            test_agent(&app, AgentId(0))
                .session
                .prompt_history
                .first()
                .map(String::as_str),
            Some("edited body")
        );
    }

    #[test]
    fn arm_send_now_during_idle_looking_wake_paints_and_arms_cancel() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::Idle;
        agent.running_wake_turn = Some(crate::app::agent_view::RunningWakeTurn {
            prompt_id: "task-completed-bg1".into(),
            cancel_sent: false,
        });
        agent.front_message_committed = true;
        agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
            id: "p-next".into(),
            version: 1,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "flush me".into(),
            position: 0,
            combined_texts: None,
        }];

        arm_send_now_and_paint(agent, "p-next", None);

        assert_eq!(agent.expect_send_now_cancel.as_deref(), Some("p-next"));
        assert!(agent.send_now_painted_blocks.contains_key("p-next"));
    }

    #[test]
    fn send_now_during_cancellation_does_not_arm_cancel_expectation() {
        for state in [AgentState::Idle, AgentState::TurnCancelling] {
            let mut app = test_app_with_agent();
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = state;
            if agent.session.state.is_idle() {
                agent.running_wake_turn = Some(crate::app::agent_view::RunningWakeTurn {
                    prompt_id: "task-completed-bg1".into(),
                    cancel_sent: true,
                });
            }
            agent.front_message_committed = true;

            arm_send_now_and_paint_dispatched(agent, "p-next", "flush me");

            assert!(agent.expect_send_now_cancel.is_none());
            assert!(!agent.send_now_painted_blocks.contains_key("p-next"));
        }
    }

    #[test]
    fn arm_send_now_skips_paint_while_front_uncommitted() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.start_turn_boundary(Some("p-front"));
        agent.session.current_prompt_id = Some("p-front".into());
        agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
            id: "p-next".into(),
            version: 1,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "flush me".into(),
            position: 0,
            combined_texts: None,
        }];
        let before = agent.scrollback.len();
        arm_send_now_and_paint(agent, "p-next", None);
        assert!(agent.expect_send_now_cancel.is_none());
        assert_eq!(agent.scrollback.len(), before);
        assert!(!agent.send_now_painted_blocks.contains_key("p-next"));

        agent.front_message_committed = true;
        arm_send_now_and_paint(agent, "p-next", None);
        assert_eq!(agent.expect_send_now_cancel.as_deref(), Some("p-next"));
        assert!(agent.send_now_painted_blocks.contains_key("p-next"));
    }

    /// The turn-start shim sets `bash_turn` and pushes no user block for an adopted `bash` entry.
    #[test]
    fn shim_bash_kind_sets_bash_turn_and_no_user_block() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        let before = agent.scrollback.len();

        let page_flip_entry = apply_turn_start_shim(
            agent,
            "p1".to_string(),
            Some("ls -la".to_string()),
            "bash",
            None,
        );

        assert!(page_flip_entry.is_none());
        assert!(agent.bash_turn, "bash adoption must set bash_turn");
        assert!(agent.session.state.is_turn_running());
        assert_eq!(agent.session.current_prompt_id.as_deref(), Some("p1"));
        // No user/display block is pushed (the shell's execute block is the entry)
        assert_eq!(agent.scrollback.len(), before);
        assert!(agent.session.in_flight_prompt.is_none());
    }

    #[test]
    fn shim_interject_fallback_reuses_interjection_bubble() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent
            .scrollback
            .push_block(RenderBlock::interjection_prompt("steer-a"));
        agent
            .scrollback
            .push_block(RenderBlock::interjection_prompt("steer-b"));
        let before = agent.scrollback.len();
        apply_turn_start_shim(
            agent,
            "interject-fallback-a".into(),
            Some("steer-a".into()),
            "prompt",
            None,
        );
        apply_turn_start_shim(
            agent,
            "interject-fallback-b".into(),
            Some("steer-b".into()),
            "prompt",
            None,
        );
        assert_eq!(agent.scrollback.len(), before);
        match &agent.scrollback.entry(0).unwrap().block {
            RenderBlock::UserPrompt(ub) => {
                assert!(!ub.is_interjection);
                assert_eq!(ub.text, "steer-a");
            }
            other => panic!("expected user bubble, got {other:?}"),
        }
        match &agent.scrollback.entry(1).unwrap().block {
            RenderBlock::UserPrompt(ub) => {
                assert!(!ub.is_interjection);
                assert_eq!(ub.text, "steer-b");
            }
            other => panic!("expected user bubble, got {other:?}"),
        }
    }

    #[test]
    fn drain_reports_page_flip_only_when_prompt_starts() {
        crate::appearance::cache::set_page_flip_on_send(true);
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.enqueue_prompt("first".into());
        let started = maybe_drain_queue(agent);
        let entry_id = started.page_flip_entry.expect("prompt starts a page flip");
        assert_eq!(
            agent.scrollback.index_of_id(entry_id),
            agent.scrollback.selected()
        );

        agent.session.enqueue_prompt("queued".into());
        let blocked = maybe_drain_queue(agent);
        assert!(blocked.effects.is_empty());
        assert!(blocked.page_flip_entry.is_none());
    }

    #[test]
    fn drain_blocked_while_hook_hold_armed() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.enqueue_prompt("queued follow-up".into());
        agent.session.hook_block_hold = true;

        let held = maybe_drain_queue(agent);
        assert!(held.effects.is_empty(), "a held queue must not drain");
        assert_eq!(
            agent.session.pending_prompts.len(),
            1,
            "the row stays queued"
        );

        agent.release_hook_block_hold();
        let drained = maybe_drain_queue(agent);
        assert!(
            !drained.effects.is_empty(),
            "a released queue drains normally"
        );
    }

    /// Turn-start path: the leader/viewer adoption shim clears the previous response's follow-up chips.
    #[test]
    fn shim_clears_follow_up_chips() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.apply_follow_ups("resp-1".into(), vec!["a".into()]);
        apply_turn_start_shim(
            agent,
            "p9".to_string(),
            Some("hi".to_string()),
            "prompt",
            None,
        );
        assert!(agent.follow_ups.is_none(), "turn adoption must clear chips");
    }

    /// Via the `queue/changed` shim: after the shim adopts a turn (setting `current_prompt_id`), a re-delivery of that turn's follow_ups re-renders.
    /// The stamped `promptId` matches the adopted turn, so chips that were applied then cleared reappear (no un-recording needed).
    #[test]
    fn shim_adopted_turn_redelivery_rerenders() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();

        // Follow-ups for the (to-be-)adopted turn p9 already applied and shown
        assert!(agent.apply_follow_ups_with_prompt("resp-1".into(), Some("p9"), vec!["a".into()]));

        // The shim adopts turn p9 (viewer: "p9" is not self-originated)
        // It clears the shown chips but keeps the seen ring and sets current_prompt_id = "p9"
        apply_turn_start_shim(
            agent,
            "p9".to_string(),
            Some("hi".to_string()),
            "prompt",
            None,
        );
        assert!(agent.attached_as_viewer, "p9 is a viewer-adopted turn");
        assert!(agent.follow_ups.is_none(), "turn adoption clears chips");
        assert!(
            agent.follow_up_seen.contains_key("resp-1"),
            "adoption keeps the seen ring"
        );

        // A re-delivery of the adopted turn's follow_ups re-renders (promptId p9 == current_prompt_id)
        let changed =
            agent.apply_follow_ups_with_prompt("resp-1".into(), Some("p9"), vec!["a".into()]);
        assert!(changed, "re-delivery of the adopted turn must re-render");
        assert_eq!(
            agent
                .follow_ups
                .as_ref()
                .expect("chips re-populated")
                .suggestions,
            vec!["a"]
        );
    }

    /// Via the shim: after starting a new turn, a buffer-replayed `x.ai/follow_ups` for a prior turn's response stays rejected (no stale revival).
    /// Its `promptId` is not the active turn and it is already seen.
    /// Covers the self-driven turn start (`p-self`).
    #[test]
    fn shim_new_turn_rejects_prior_turn_replay() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();

        // A prior turn (p-prev) produced resp-1's chips.
        agent.session.current_prompt_id = Some("p-prev".into());
        assert!(agent.apply_follow_ups_with_prompt(
            "resp-1".into(),
            Some("p-prev"),
            vec!["a".into()]
        ));

        // This client starts a new self-driven turn (p-self).
        agent.note_self_originated_prompt("p-self");
        apply_turn_start_shim(
            agent,
            "p-self".to_string(),
            Some("hi".to_string()),
            "prompt",
            None,
        );
        assert!(
            !agent.attached_as_viewer,
            "a self-originated turn is driver, not viewer"
        );
        assert!(agent.follow_ups.is_none(), "turn start hides chips");
        assert!(
            agent.follow_up_seen.contains_key("resp-1"),
            "the seen ring entry for the prior response is kept"
        );

        // A replayed prior turn (p-prev/resp-1) must not revive on the new turn
        let changed =
            agent.apply_follow_ups_with_prompt("resp-1".into(), Some("p-prev"), vec!["a".into()]);
        assert!(
            !changed,
            "a replayed prior-turn follow_ups must stay rejected on the new turn"
        );
        assert!(
            agent.follow_ups.is_none(),
            "no stale chips may resurface from the replay"
        );
    }

    /// A viewer adopting a synthetic/cron turn (no shared-queue text, so no local block) must clear the `expect_user_echo` flag `start_turn` set.
    /// The agent's live user-message broadcast (the only source of the cron `↻` header) then renders instead of being swallowed.
    /// When the shim does render a block (queue text present), it keeps the skip so the broadcast isn't duplicated.
    #[test]
    fn shim_clears_echo_skip_only_when_it_renders_no_block() {
        // No queue text means no local block: clear the skip (let broadcast render)
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        apply_turn_start_shim(agent, "cron-1".to_string(), None, "prompt", None);
        assert!(
            !agent.session.tracker.expects_user_echo(),
            "no-block adoption must clear the echo skip so the cron broadcast renders"
        );

        // Queue text present means a local block is rendered: keep the skip (no dup)
        let mut app2 = test_app_with_agent();
        let agent2 = app2.agents.get_mut(&AgentId(0)).unwrap();
        apply_turn_start_shim(
            agent2,
            "p2".to_string(),
            Some("hello world".to_string()),
            "prompt",
            None,
        );
        assert!(
            agent2.session.tracker.expects_user_echo(),
            "rendering the user block locally must keep the skip to avoid duplicating the broadcast"
        );
    }

    #[test]
    fn send_now_shim_skips_scroll_to_entry_top() {
        // This test exercises the send-now exception within the page-flip behavior, so pin the setting on (the cache is thread-local)
        crate::appearance::cache::set_page_flip_on_send(true);
        fn seed_tall_scrollback(agent: &mut crate::app::agent_view::AgentView) -> usize {
            for i in 0..40 {
                agent
                    .scrollback
                    .push_block(RenderBlock::agent_message(format!(
                        "seeded assistant line {i}: filler so entry-top and bottom offsets diverge"
                    )));
            }
            agent.scrollback.prepare_layout(80, 8);
            agent.scrollback.goto_bottom();
            agent.scrollback.scroll_offset()
        }

        let mut app_normal = test_app_with_agent();
        let agent_n = app_normal.agents.get_mut(&AgentId(0)).unwrap();
        let bottom_n = seed_tall_scrollback(agent_n);
        agent_n.note_self_originated_prompt("p-normal");
        apply_turn_start_shim(
            agent_n,
            "p-normal".into(),
            Some("normal next".into()),
            "prompt",
            None,
        );
        let normal_offset = agent_n.scrollback.scroll_offset();
        assert!(agent_n.scrollback.is_follow_mode());
        assert_ne!(
            normal_offset, bottom_n,
            "normal adoption should leave bottom via scroll_to_entry_top"
        );

        let mut app_send = test_app_with_agent();
        let agent_s = app_send.agents.get_mut(&AgentId(0)).unwrap();
        seed_tall_scrollback(agent_s);
        agent_s.note_self_originated_prompt("p-send-now");
        agent_s.arm_send_now_expectation("p-send-now".into());
        assert!(agent_s.follow_without_jump_prompt_id.is_some());
        // Cancel-rail take: only sticky pin remains.
        let _ = agent_s.expect_send_now_cancel.take();
        apply_turn_start_shim(
            agent_s,
            "p-send-now".into(),
            Some("hurry up".into()),
            "prompt",
            None,
        );
        assert!(agent_s.follow_without_jump_prompt_id.is_none());
        assert!(agent_s.scrollback.is_follow_mode());
        let send_now_offset = agent_s.scrollback.scroll_offset();
        assert_ne!(
            send_now_offset, normal_offset,
            "send-now must not use scroll_to_entry_top"
        );

        // Miss: armed for A, adopt B; entry-top path, pin still dropped
        let mut app_miss = test_app_with_agent();
        let agent_m = app_miss.agents.get_mut(&AgentId(0)).unwrap();
        let bottom_m = seed_tall_scrollback(agent_m);
        agent_m.note_self_originated_prompt("p-b");
        agent_m.arm_send_now_expectation("p-a".into());
        apply_turn_start_shim(agent_m, "p-b".into(), Some("other".into()), "prompt", None);
        assert!(agent_m.follow_without_jump_prompt_id.is_none());
        assert_ne!(
            agent_m.scrollback.scroll_offset(),
            bottom_m,
            "mismatched arm must still scroll_to_entry_top"
        );
    }

    /// Shell user-echo can paint the prompt before the deferred turn-start shim runs.
    /// Reuse that trailing block; do not push a second copy.
    #[test]
    fn shim_reuses_already_painted_user_prompt() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-dup");
        // Simulate the shell echo winning the race.
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("/deslop"));
        let before = agent.scrollback.len();
        apply_turn_start_shim(
            agent,
            "p-dup".into(),
            Some("/deslop".into()),
            "prompt",
            None,
        );
        assert_eq!(
            agent.scrollback.len(),
            before,
            "must not push a second user-prompt block when echo already painted"
        );
        let last = agent.scrollback.entry(before - 1).expect("trailing entry");
        match &last.block {
            RenderBlock::UserPrompt(ub) => assert_eq!(ub.text, "/deslop"),
            other => panic!("expected user prompt, got {other:?}"),
        }
    }

    #[test]
    fn shim_paints_one_bubble_per_combined_segment() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-combo");
        let before = agent.scrollback.len();
        apply_turn_start_shim(
            agent,
            "p-combo".into(),
            Some("first\n\nsecond".into()),
            "prompt",
            Some(vec!["first".into(), "second".into()]),
        );
        assert_eq!(agent.scrollback.len(), before + 2);
        assert_eq!(user_prompt_count(agent, "first"), 1);
        assert_eq!(user_prompt_count(agent, "second"), 1);
        assert_eq!(user_prompt_count(agent, "first\n\nsecond"), 0);
        assert_eq!(
            agent.session.in_flight_prompt.as_ref().unwrap().text,
            "first\n\nsecond"
        );
    }

    #[test]
    fn shim_replaces_joined_echo_with_multi_bubbles() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-combo");
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("first\n\nsecond"));
        apply_turn_start_shim(
            agent,
            "p-combo".into(),
            Some("first\n\nsecond".into()),
            "prompt",
            Some(vec!["first".into(), "second".into()]),
        );
        assert_eq!(user_prompt_count(agent, "first"), 1);
        assert_eq!(user_prompt_count(agent, "second"), 1);
        assert_eq!(user_prompt_count(agent, "first\n\nsecond"), 0);
    }

    #[test]
    fn shim_reuses_already_painted_combined_segments() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-combo");
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("first"));
        agent
            .scrollback
            .push_block(RenderBlock::user_prompt("second"));
        let before = agent.scrollback.len();
        apply_turn_start_shim(
            agent,
            "p-combo".into(),
            Some("first\n\nsecond".into()),
            "prompt",
            Some(vec!["first".into(), "second".into()]),
        );
        assert_eq!(agent.scrollback.len(), before);
        assert_eq!(user_prompt_count(agent, "first"), 1);
        assert_eq!(user_prompt_count(agent, "second"), 1);
    }

    /// Number of `UserPrompt` blocks with exactly `text`.
    fn user_prompt_count(agent: &crate::app::agent_view::AgentView, text: &str) -> usize {
        (0..agent.scrollback.len())
            .filter_map(|i| agent.scrollback.entry(i))
            .filter(|e| matches!(&e.block, RenderBlock::UserPrompt(ub) if ub.text == text))
            .count()
    }

    /// Queue-row send-now paints at dispatch; the adoption reuses the block.
    #[test]
    fn queue_interject_shared_paints_user_block_at_arm() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
                id: "p-ty".into(),
                version: 1,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "ty".into(),
                position: 0,
                combined_texts: None,
            }];
        }
        let effects = dispatch(
            Action::QueueInterjectShared {
                id: "p-ty".into(),
                expected_version: 1,
                new_text: None,
            },
            &mut app,
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::QueueInterject { .. }]
        ));
        let agent = app.agents.get_mut(&id).unwrap();
        assert_eq!(
            user_prompt_count(agent, "ty"),
            1,
            "send-now must paint the user block at dispatch"
        );
        // The adoption shim reuses the painted block instead of double-pushing.
        agent.note_self_originated_prompt("p-ty");
        apply_turn_start_shim(agent, "p-ty".into(), Some("ty".into()), "prompt", None);
        assert_eq!(
            user_prompt_count(agent, "ty"),
            1,
            "turn-start adoption must reuse the dispatch-painted block"
        );
    }

    /// No paint when idle (adoption renders the drain) or for bash rows.
    #[test]
    fn queue_interject_shared_skips_paint_when_not_arming_or_bash() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().shared_queue =
            vec![crate::app::prompt_queue::QueueEntryWire {
                id: "p-idle".into(),
                version: 1,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "idle row".into(),
                position: 0,
                combined_texts: None,
            }];
        // Idle (no running turn): expects_send_now_cancel is false, so no arm and no paint
        let _ = dispatch(
            Action::QueueInterjectShared {
                id: "p-idle".into(),
                expected_version: 1,
                new_text: None,
            },
            &mut app,
        );
        assert_eq!(user_prompt_count(test_agent(&app, id), "idle row"), 0);

        // Bash row mid-turn: armed, but its adoption paints no user block.
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
                id: "p-bash".into(),
                version: 1,
                owner: None,
                last_editor: None,
                kind: "bash".into(),
                text: "ls -la".into(),
                position: 0,
                combined_texts: None,
            }];
        }
        let _ = dispatch(
            Action::QueueInterjectShared {
                id: "p-bash".into(),
                expected_version: 1,
                new_text: None,
            },
            &mut app,
        );
        assert_eq!(user_prompt_count(test_agent(&app, id), "ls -la"), 0);
    }

    /// Composer/local-row send-now paints at dispatch too.
    #[test]
    fn send_prompt_now_paints_user_block_at_arm() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        let effects = dispatch(
            Action::SendPromptNow {
                text: "hurry".into(),
                images: vec![],
            },
            &mut app,
        );
        assert!(matches!(effects.as_slice(), [Effect::SendPromptNow { .. }]));
        assert_eq!(user_prompt_count(test_agent(&app, id), "hurry"), 1);
    }

    /// The reuse scan looks past turn-boundary chrome landing between the paint and the adoption (interject no-op, natural drain).
    #[test]
    fn shim_reuses_painted_block_past_turn_chrome() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-late");
        push_send_now_user_block(agent, "p-late", "prompt", "ty", false);
        agent.scrollback.push_block(RenderBlock::session_event(
            crate::scrollback::blocks::SessionEvent::TurnCompleted {
                elapsed: Some(std::time::Duration::from_secs(2)),
            },
        ));
        agent
            .scrollback
            .push_block(RenderBlock::system("connection restored"));
        apply_turn_start_shim(agent, "p-late".into(), Some("ty".into()), "prompt", None);
        assert_eq!(
            user_prompt_count(agent, "ty"),
            1,
            "reuse must look past trailing SessionEvent/System chrome"
        );
        // A content block ends the scan.
        agent
            .scrollback
            .push_block(RenderBlock::agent_message("done"));
        agent.note_self_originated_prompt("p-again");
        apply_turn_start_shim(agent, "p-again".into(), Some("ty".into()), "prompt", None);
        assert_eq!(
            user_prompt_count(agent, "ty"),
            2,
            "content between ends the reuse scan"
        );
    }

    /// Idempotent per prompt id; new-text re-arm replaces; blank rows paint (their adoption renders a blank bubble too).
    #[test]
    fn push_send_now_user_block_dedupes_and_replaces_on_new_text() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        push_send_now_user_block(agent, "p-1", "prompt", "ty", false);
        push_send_now_user_block(agent, "p-1", "prompt", "ty", false);
        assert_eq!(user_prompt_count(agent, "ty"), 1);
        push_send_now_user_block(agent, "p-1", "prompt", "ty edited", true);
        assert_eq!(user_prompt_count(agent, "ty"), 0);
        assert_eq!(user_prompt_count(agent, "ty edited"), 1);
        let before = agent.scrollback.len();
        push_send_now_user_block(agent, "p-2", "prompt", "   ", false);
        assert_eq!(agent.scrollback.len(), before + 1, "blank rows paint too");
    }

    /// Stacked send-nows: each adoption reuses its own id-keyed block.
    #[test]
    fn stacked_send_nows_each_reuse_their_own_block() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-thx");
        agent.note_self_originated_prompt("p-ty");
        push_send_now_user_block(agent, "p-thx", "prompt", "thx", false);
        push_send_now_user_block(agent, "p-ty", "prompt", "ty", false);
        apply_turn_start_shim(agent, "p-thx".into(), Some("thx".into()), "prompt", None);
        assert_eq!(user_prompt_count(agent, "thx"), 1);
        agent
            .scrollback
            .push_block(RenderBlock::agent_message("You're welcome."));
        apply_turn_start_shim(agent, "p-ty".into(), Some("ty".into()), "prompt", None);
        assert_eq!(user_prompt_count(agent, "ty"), 1);
    }

    /// Identical-text stacked send-nows get one block each (no aliasing).
    #[test]
    fn stacked_identical_text_send_nows_get_distinct_blocks() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        push_send_now_user_block(agent, "p-1", "prompt", "go", false);
        push_send_now_user_block(agent, "p-2", "prompt", "go", false);
        assert_eq!(user_prompt_count(agent, "go"), 2);
        assert_ne!(
            agent.send_now_painted_blocks.get("p-1").map(|v| v.0),
            agent.send_now_painted_blocks.get("p-2").map(|v| v.0)
        );
        // The sibling's block must survive the first adoption's dup sweep.
        agent.note_self_originated_prompt("p-1");
        agent.note_self_originated_prompt("p-2");
        apply_turn_start_shim(agent, "p-1".into(), Some("go".into()), "prompt", None);
        assert_eq!(user_prompt_count(agent, "go"), 2);
        apply_turn_start_shim(agent, "p-2".into(), Some("go".into()), "prompt", None);
        assert_eq!(user_prompt_count(agent, "go"), 2);
        assert!(agent.send_now_painted_blocks.is_empty());
    }

    /// Viewers never paint (their echo is the block source).
    #[test]
    fn viewer_send_now_does_not_paint() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.attached_as_viewer = true;
        push_send_now_user_block(agent, "p-v", "prompt", "hi", false);
        assert_eq!(user_prompt_count(agent, "hi"), 0);
        assert!(agent.send_now_painted_blocks.is_empty());
    }

    /// Drifted non-edited paint is swapped for the adoption's text.
    #[test]
    fn shim_swaps_stale_painted_text_for_adoption_text() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-edit");
        push_send_now_user_block(agent, "p-edit", "prompt", "old text", false);
        apply_turn_start_shim(
            agent,
            "p-edit".into(),
            Some("new text".into()),
            "prompt",
            None,
        );
        assert_eq!(user_prompt_count(agent, "old text"), 0);
        assert_eq!(user_prompt_count(agent, "new text"), 1);
    }

    /// Edited paint outranks the adoption's stale mirror text.
    #[test]
    fn shim_keeps_edited_paint_over_stale_adoption_text() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
                id: "p-ed".into(),
                version: 1,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "original".into(),
                position: 0,
                combined_texts: None,
            }];
        }
        let _ = dispatch(
            Action::QueueInterjectShared {
                id: "p-ed".into(),
                expected_version: 1,
                new_text: Some("edited body".into()),
            },
            &mut app,
        );
        let agent = app.agents.get_mut(&id).unwrap();
        assert_eq!(
            user_prompt_count(agent, "edited body"),
            1,
            "the paint must show the edited text the shell will run"
        );
        // Adoption captures the pre-edit mirror text; the edited paint wins.
        agent.note_self_originated_prompt("p-ed");
        apply_turn_start_shim(
            agent,
            "p-ed".into(),
            Some("original".into()),
            "prompt",
            None,
        );
        assert_eq!(user_prompt_count(agent, "edited body"), 1);
        assert_eq!(user_prompt_count(agent, "original"), 0);
        // The Ctrl+C rewind restore must match the on-screen (edited) text.
        assert_eq!(
            agent
                .session
                .in_flight_prompt
                .as_ref()
                .map(|p| p.text.as_str()),
            Some("edited body")
        );
    }

    /// An early unarmed echo's duplicate is swept at adoption.
    #[test]
    fn shim_drops_echo_duplicate_of_painted_block() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.note_self_originated_prompt("p-echo");
        push_send_now_user_block(agent, "p-echo", "prompt", "ty", false);
        // Echo slips in before the adoption (no promptId, skip not armed yet).
        agent.scrollback.push_block(RenderBlock::user_prompt("ty"));
        apply_turn_start_shim(agent, "p-echo".into(), Some("ty".into()), "prompt", None);
        assert_eq!(user_prompt_count(agent, "ty"), 1);
    }

    fn deliver_user_echo(app: &mut crate::app::app_view::AppView, text: &str) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::SessionNotification::new(
            acp::SessionId::new("test-session"),
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
        );
        let _ = crate::app::acp_handler::handle(
            distill_acp_lib::AcpClientMessage::SessionNotification(distill_acp_lib::AcpArgs {
                request,
                response_tx: tx,
            }),
            app,
        );
    }

    #[test]
    fn send_now_paint_swallows_racing_user_echo() {
        let mut app = test_app_with_agent();
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.note_self_originated_prompt("p-first");
            push_send_now_user_block(agent, "p-first", "prompt", "first queued", false);
        }

        deliver_user_echo(&mut app, "first queued");
        assert_eq!(
            user_prompt_count(test_agent(&app, AgentId(0)), "first queued"),
            1
        );

        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            apply_turn_start_shim(
                agent,
                "p-first".into(),
                Some("first queued".into()),
                "prompt",
                None,
            );
        }
        assert_eq!(
            user_prompt_count(test_agent(&app, AgentId(0)), "first queued"),
            1
        );

        deliver_user_echo(&mut app, "next real message");
        assert_eq!(
            user_prompt_count(test_agent(&app, AgentId(0)), "next real message"),
            1
        );
    }

    #[test]
    fn send_now_echo_matches_trimmed_text_and_expires_same_text_paints() {
        let mut app = test_app_with_agent();
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            push_send_now_user_block(agent, "p-ws", "prompt", "hello\n", false);
        }
        deliver_user_echo(&mut app, "hello");
        assert_eq!(
            user_prompt_count(test_agent(&app, AgentId(0)), "hello\n"),
            1
        );
        deliver_user_echo(&mut app, "hello");
        assert_eq!(user_prompt_count(test_agent(&app, AgentId(0)), "hello"), 1);

        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            push_send_now_user_block(agent, "p-a", "prompt", "same", false);
            push_send_now_user_block(agent, "p-b", "prompt", "same", false);
        }
        deliver_user_echo(&mut app, "same");
        deliver_user_echo(&mut app, "same");
        assert_eq!(user_prompt_count(test_agent(&app, AgentId(0)), "same"), 2);
        deliver_user_echo(&mut app, "same");
        assert_eq!(user_prompt_count(test_agent(&app, AgentId(0)), "same"), 3);
    }

    #[test]
    fn send_now_echo_follows_adoption_text_when_paint_drifts() {
        let mut app = test_app_with_agent();
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.note_self_originated_prompt("p-drift");
            push_send_now_user_block(agent, "p-drift", "prompt", "old paint", false);
            apply_turn_start_shim(
                agent,
                "p-drift".into(),
                Some("fresh".into()),
                "prompt",
                None,
            );
        }
        assert_eq!(
            user_prompt_count(test_agent(&app, AgentId(0)), "old paint"),
            0
        );
        assert_eq!(user_prompt_count(test_agent(&app, AgentId(0)), "fresh"), 1);
        deliver_user_echo(&mut app, "fresh");
        assert_eq!(user_prompt_count(test_agent(&app, AgentId(0)), "fresh"), 1);
        deliver_user_echo(&mut app, "fresh");
        assert_eq!(user_prompt_count(test_agent(&app, AgentId(0)), "fresh"), 2);
    }

    /// A painted-pending row stays hidden after the arm drops; the pair resolves at adoption or retire, never by the arm's lifetime.
    #[test]
    fn painted_pending_row_stays_hidden_after_arm_drop() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
            id: "p-drop".into(),
            version: 1,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "held".into(),
            position: 0,
            combined_texts: None,
        }];
        arm_send_now_and_paint(agent, "p-drop", None);
        assert!(agent.visible_queue_is_empty(), "armed row is hidden");
        // Interactive cancel drops the arm; the block still owns the message.
        agent.clear_send_now_expectation();
        assert!(
            agent.visible_queue_is_empty(),
            "painted-pending row must stay hidden after the arm drops"
        );
        assert_eq!(user_prompt_count(agent, "held"), 1);
        // Retiring resolves both: block gone, row visible again.
        agent.retire_send_now_painted_block("p-drop");
        assert!(!agent.visible_queue_is_empty());
        assert_eq!(user_prompt_count(agent, "held"), 0);
    }

    /// The adoption's text fallback must not claim a block owned by another pending send-now (map-less adoption racing a painted sibling).
    #[test]
    fn shim_fallback_skips_blocks_claimed_by_pending_send_nows() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        push_send_now_user_block(agent, "p-owned", "prompt", "go", false);
        // p-other adopts with the same text but no map entry of its own.
        agent.note_self_originated_prompt("p-other");
        apply_turn_start_shim(agent, "p-other".into(), Some("go".into()), "prompt", None);
        assert_eq!(
            user_prompt_count(agent, "go"),
            2,
            "the fallback must push fresh instead of claiming p-owned's block"
        );
        assert!(agent.send_now_painted_blocks.contains_key("p-owned"));
    }

    /// A never-run send-now takes its block with it (no requeue duplicate).
    #[test]
    fn retire_send_now_painted_block_removes_ghost() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        push_send_now_user_block(agent, "p-fail", "prompt", "lost?", false);
        assert_eq!(user_prompt_count(agent, "lost?"), 1);
        agent.retire_send_now_painted_block("p-fail");
        assert_eq!(user_prompt_count(agent, "lost?"), 0);
        assert!(agent.send_now_painted_blocks.is_empty());
    }

    /// The cancel-and-send arm survives the *matching* prompt's turn start, so the cancel rail can still suppress the marker when racing after adopt.
    /// A non-matching (stale) arm is dropped.
    #[test]
    fn start_turn_boundary_keeps_send_now_cancel_expectation() {
        let mut app = test_app_with_agent();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.arm_send_now_expectation("p-send-now".into());
        agent.start_turn_boundary(Some("p-send-now"));
        assert_eq!(
            agent.expect_send_now_cancel.as_deref(),
            Some("p-send-now"),
            "adopt must not clear the cancel-marker arm before the cancel PromptResponse"
        );
        assert!(
            agent.follow_without_jump_prompt_id.is_some(),
            "sticky pin is independent of start_turn_boundary"
        );
        // A different prompt starting the turn means the arm is stale; drop it so it cannot suppress that turn's later real cancel marker
        agent.start_turn_boundary(Some("p-other"));
        assert!(
            agent.expect_send_now_cancel.is_none(),
            "a non-matching turn start clears the stale send-now arm"
        );
    }

    #[test]
    fn drain_queue_when_running_does_nothing() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "queued");

        // DrainQueue while running has no effect
        let effects = dispatch(Action::DrainQueue, &mut app);
        assert!(effects.is_empty());
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);
    }

    #[test]
    fn drain_queue_blocked_during_loading_replay() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // Simulate session-resume state: Idle but still replaying.
        app.agents.get_mut(&id).unwrap().session.loading_replay = true;
        enqueue_local(&mut app, id, "check status");

        // A drain while loading_replay is true must be blocked
        let effects = maybe_drain_queue(app.agents.get_mut(&id).unwrap()).effects;
        assert!(
            effects.is_empty(),
            "drain must be blocked during loading_replay"
        );
        assert_eq!(
            test_agent(&app, id).session.queue_len(),
            1,
            "the prompt must stay queued"
        );

        // Clear loading_replay (simulates SessionLoaded completing).
        app.agents.get_mut(&id).unwrap().session.loading_replay = false;

        // Drain again: it succeeds now
        let effects = maybe_drain_queue(app.agents.get_mut(&id).unwrap()).effects;
        assert_eq!(effects.len(), 1);
        assert_eq!(
            test_agent(&app, id).session.queue_len(),
            0,
            "queue must be empty after drain"
        );
    }

    #[test]
    fn drain_after_editing_sends_correct_prompt() {
        // Regression: editing #3, prompts #1 and #2 drain, #3 becomes front.
        // User presses Enter (save): DrainQueue must send #3's updated text, not #4 or the old text
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // Queue 4 prompts, first drains.
        dispatch(Action::SendPrompt("p1".into()), &mut app);
        enqueue_local(&mut app, id, "p2");
        enqueue_local(&mut app, id, "p3");
        enqueue_local(&mut app, id, "p4");
        assert_eq!(test_agent(&app, id).session.queue_len(), 3); // p2, p3, p4

        // Ending the turn for p1 sets Idle, maybe_drain_queue pops p2, and the state is Running again
        // Queue is now: p3, p4.
        dispatch(end_turn(), &mut app);
        assert_eq!(test_agent(&app, id).session.queue_len(), 2);

        // Start editing p3 (now front).
        let Some(p3_id) = test_agent(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.id)
        else {
            panic!("expected queued prompt");
        };
        app.agents.get_mut(&id).unwrap().prompt_mode = PromptMode::EditingQueued {
            id: p3_id,
            original: "p3".into(),
            server_id: None,
            kind: crate::app::agent::QueueEntryKind::Prompt,
        };

        // End turn for p2: must not drain p3 (being edited), only FetchBilling
        let effects = dispatch(end_turn(), &mut app);
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(
                effects.first(),
                Some(Effect::FetchBilling { silent: true, .. })
            ),
            "drain should be blocked, only billing refresh"
        );
        assert_eq!(test_agent(&app, id).session.queue_len(), 2); // p3, p4

        // Simulate user saving edited text.
        app.agents
            .get_mut(&id)
            .unwrap()
            .session
            .pending_prompts
            .iter_mut()
            .find(|p| p.id == p3_id)
            .unwrap()
            .text = "p3-edited".into();
        app.agents.get_mut(&id).unwrap().prompt_mode = PromptMode::Normal;

        // DrainQueue after the edit sends "p3-edited", not "p4"
        let effects = dispatch(Action::DrainQueue, &mut app);
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "p3-edited"),
            "should send the edited prompt"
        );
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);
        assert_eq!(
            test_agent(&app, id)
                .session
                .pending_prompts
                .front()
                .map(|p| p.text.as_str()),
            Some("p4")
        );
    }

    #[test]
    fn drain_queue_blocked_during_reconnect() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // Enqueue a prompt while not reconnecting so it's queued.
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "queued");
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);

        // Set idle and reconnect_pending: DrainQueue is blocked
        app.agents.get_mut(&id).unwrap().session.state = AgentState::Idle;
        app.reconnect_pending = true;

        let effects = dispatch(Action::DrainQueue, &mut app);
        assert!(effects.is_empty());
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);
    }

    /// Regression (leader mode): a prompt queued during a turn must drain once the leader connection reconnects.
    /// Every normal drain trigger (PromptResponse / DrainQueue / send-prompt / session-created) early-returns while `reconnect_pending` is set.
    /// This test exercises that final drain step and guards against the queue silently stalling after a reconnect.
    #[test]
    fn drain_queue_after_reconnect_sends_queued_prompt() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        // A prompt was queued behind a running turn before the outage.
        app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
        enqueue_local(&mut app, id, "queued");
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);

        // During the outage every drain trigger is suppressed.
        app.reconnect_pending = true;
        let blocked = dispatch(Action::DrainQueue, &mut app);
        assert!(blocked.is_empty(), "drain must stay blocked mid-reconnect");
        assert_eq!(test_agent(&app, id).session.queue_len(), 1);

        // Reconnect completes: the event loop clears `reconnect_pending` and force-idles the agent, then dispatches DrainQueue
        // Mirror that here
        app.reconnect_pending = false;
        app.agents.get_mut(&id).unwrap().session.state = AgentState::Idle;

        let effects = dispatch(Action::DrainQueue, &mut app);
        assert_eq!(
            effects.len(),
            1,
            "queued prompt must drain once reconnect clears"
        );
        assert!(matches!(effects.first(), Some(Effect::SendPrompt { .. })));
        assert!(test_agent(&app, id).session.state.is_turn_running());
        assert_eq!(
            test_agent(&app, id).session.queue_len(),
            0,
            "queue must be empty after the post-reconnect drain"
        );
    }

    #[test]
    fn parked_wait_renders_parked_without_markers() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

        let agent = app.agents.get_mut(&id).unwrap();
        assert!(agent.renders_parked(), "live wait = parked look");
        assert_eq!(count_turn_markers(agent), 0, "a park writes no marker");
    }

    #[test]
    fn sibling_batch_park_writes_no_markers() {
        use crate::acp::meta::NotificationMeta;
        use std::sync::Arc;

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        let agent = app.agents.get_mut(&id).unwrap();

        // Blocking get_task_output registers first; anchor still running.
        simulate_task_output_wait_call(agent, "wait-1", "bg-anchor", 120_000);
        agent
            .session
            .bg_tasks
            .insert("bg-anchor".into(), running_bg_task("bg-anchor"));
        assert_eq!(count_turn_markers(agent), 0);

        for i in 2..=5 {
            let tc_id = format!("wait-batch-tc{i}");
            agent.session.handle_update(
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(
                        acp::ToolCallId::new(Arc::from(tc_id.as_str())),
                        "run_terminal_command",
                    )
                    .kind(acp::ToolKind::Execute)
                    .status(acp::ToolCallStatus::Pending),
                ),
                &NotificationMeta::default(),
                &mut agent.scrollback,
            );
        }
        assert_eq!(
            count_turn_markers(agent),
            0,
            "a sibling-batch park must write zero markers"
        );
    }

    #[test]
    fn chips_and_completions_mid_park_add_no_markers() {
        use crate::scrollback::block::RenderBlock;

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        let agent = app.agents.get_mut(&id).unwrap();

        simulate_task_output_wait_call(agent, "wait-1", "bg-1", 30_000);
        assert_eq!(count_turn_markers(agent), 0);

        agent.scrollback.push_block(RenderBlock::bg_task_completed(
            "sleep 5",
            "bg-2",
            std::time::Duration::from_secs(5),
        ));
        assert_eq!(count_turn_markers(agent), 0, "chips add no markers");
        assert!(agent.renders_parked(), "chips keep the parked look");
    }

    #[test]
    fn wait_completion_clears_parked_look() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        let agent = app.agents.get_mut(&id).unwrap();

        simulate_task_output_wait_call(agent, "wait-1", "bg-1", 15_000);
        assert!(agent.renders_parked());

        complete_task_output_wait_call(agent, "wait-1");
        assert!(!agent.renders_parked(), "no parked look between parks");
        assert_eq!(count_turn_markers(agent), 0);

        simulate_task_output_wait_call(agent, "wait-2", "bg-1", 15_000);
        assert!(agent.renders_parked(), "a re-park flips the look back on");
        assert_eq!(count_turn_markers(agent), 0, "re-parks stay markerless");
    }

    #[test]
    fn interjection_during_park_adds_no_marker() {
        use crate::scrollback::block::RenderBlock;

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");
        assert!(test_agent(&app, id).renders_parked());

        let _ = dispatch(
            Action::Interject {
                text: "hurry up".into(),
                images: Vec::new(),
            },
            &mut app,
        );

        let agent = test_agent(&app, id);
        assert_eq!(
            count_turn_markers(agent),
            0,
            "no marker around the interjection"
        );
        assert!(
            matches!(
                agent.scrollback.last().map(|e| &e.block),
                Some(RenderBlock::UserPrompt(_))
            ),
            "the user prompt row lands with nothing under it"
        );
    }

    #[test]
    fn parked_wait_holds_queue_and_explains_itself() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        enqueue_local(&mut app, id, "queued row");
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

        let agent = app.agents.get_mut(&id).unwrap();
        assert_eq!(count_turn_markers(agent), 0);
        assert!(
            agent.renders_parked(),
            "the parked look is queue-occupancy-independent"
        );
        assert_eq!(
            agent.held_queue_count(),
            1,
            "held row feeds the inline status hint"
        );

        assert_eq!(agent.session.pending_prompts.len(), 1);
    }

    /// A foreground-subagent wait is sendable but never parks (parent blocked, not completed).
    #[test]
    fn subagent_wait_holds_queue_but_never_parks() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        enqueue_local(&mut app, id, "queued row");
        simulate_subagent_wait(app.agents.get_mut(&id).unwrap());

        let agent = app.agents.get_mut(&id).unwrap();
        assert_eq!(
            agent.held_queue_count(),
            1,
            "held row feeds the inline status hint"
        );
        assert_eq!(count_turn_markers(agent), 0);
        assert!(!agent.renders_parked());

        // Even with an empty queue and a live subagent, a subagent wait never parks
        agent
            .subagent_sessions
            .insert("child-1".into(), running_subagent_info("child-1"));
        agent.session.pending_prompts.clear();
        assert_eq!(
            count_turn_markers(agent),
            0,
            "subagent wait must never park"
        );
        assert!(
            !agent.renders_parked(),
            "subagent wait keeps running chrome"
        );
    }

    /// A live chunk must un-park even when the wait's terminal ToolCallUpdate never reached this client.
    #[test]
    fn parked_look_clears_when_model_resumes_streaming() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

        let agent = app.agents.get_mut(&id).unwrap();
        assert!(agent.renders_parked(), "parked look active during the wait");

        // The model resumes with a message chunk (no Completed for the wait).
        let meta = crate::acp::meta::NotificationMeta::default();
        agent.session.handle_update(
            agent_client_protocol::SessionUpdate::AgentMessageChunk(
                agent_client_protocol::ContentChunk::new(
                    agent_client_protocol::ContentBlock::Text(
                        agent_client_protocol::TextContent::new("resuming".to_string()),
                    ),
                ),
            ),
            &meta,
            &mut agent.scrollback,
        );

        assert!(
            !agent.renders_parked(),
            "a live stream must un-park the chrome (spinner returns)"
        );
        assert!(
            !agent.is_parked_on_sendable_wait(),
            "the stale wait must not survive a resumed stream"
        );

        // A new-stream thought (different stream_start_ms) also un-parks; same-stream must not.
        {
            let wait_meta = crate::acp::meta::NotificationMeta {
                stream_start_ms: Some(1),
                ..Default::default()
            };
            agent.session.handle_update(
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(
                        acp::ToolCallId::new(std::sync::Arc::from("wait-2")),
                        "get_command_or_subagent_output",
                    )
                    .kind(acp::ToolKind::Other)
                    .status(acp::ToolCallStatus::Pending)
                    .content(vec![])
                    .locations(vec![]),
                ),
                &wait_meta,
                &mut agent.scrollback,
            );
            agent.session.handle_update(
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    acp::ToolCallId::new(std::sync::Arc::from("wait-2")),
                    acp::ToolCallUpdateFields::new().raw_input(Some(serde_json::json!({
                        "task_ids": ["bg-2"],
                        "timeout_ms": 30_000,
                    }))),
                )),
                &wait_meta,
                &mut agent.scrollback,
            );
        }
        assert!(agent.is_parked_on_sendable_wait());
        let thought_meta = crate::acp::meta::NotificationMeta {
            stream_start_ms: Some(9_001),
            ..Default::default()
        };
        agent.session.handle_update(
            agent_client_protocol::SessionUpdate::AgentThoughtChunk(
                agent_client_protocol::ContentChunk::new(
                    agent_client_protocol::ContentBlock::Text(
                        agent_client_protocol::TextContent::new("thinking again".to_string()),
                    ),
                ),
            ),
            &thought_meta,
            &mut agent.scrollback,
        );
        assert!(
            !agent.is_parked_on_sendable_wait(),
            "a new-stream thought must clear the stale wait"
        );
    }

    /// The hint must only advertise "Enter to send now" when the top held row would actually send (a bash top row no-ops).
    #[test]
    fn held_hint_advertises_send_now_only_for_sendable_top() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.enqueue_bash_command("git status".into());
        assert_eq!(agent.held_queue_count(), 1);
        assert!(
            !agent.held_queue_top_sendable(),
            "a bash top row must not advertise Enter-send-now"
        );

        agent.session.pending_prompts.clear();
        agent.session.enqueue_prompt("plain follow-up".into());
        assert!(agent.held_queue_top_sendable());

        // A server row (renders first in the merge) is always sendable.
        agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
            id: "srv-1".into(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "bash".into(),
            text: "server bash".into(),
            position: 0,
            combined_texts: None,
        }];
        agent.session.pending_prompts.clear();
        agent.session.enqueue_bash_command("still bash".into());
        assert!(
            agent.held_queue_top_sendable(),
            "a server top row sends now regardless of kind"
        );
    }

    #[test]
    fn held_queue_count_matches_pane_with_send_now_echo() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

        let agent = app.agents.get_mut(&id).unwrap();
        let running = agent
            .session
            .current_prompt_id
            .clone()
            .unwrap_or_else(|| "running".into());
        agent.expect_send_now_cancel = Some("send-now-echo".into());
        agent.shared_queue = vec![
            crate::app::prompt_queue::QueueEntryWire {
                id: "send-now-echo".into(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "send now payload".into(),
                position: 0,
                combined_texts: None,
            },
            crate::app::prompt_queue::QueueEntryWire {
                id: "held-1".into(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "genuine held".into(),
                position: 1,
                combined_texts: None,
            },
        ];
        agent.session.current_prompt_id = Some(running);
        agent.session.pending_prompts.clear();

        assert_eq!(
            agent.held_queue_count(),
            1,
            "hint counts only the genuine held row"
        );
        agent.sync_queue_pane();
        let pane_len = agent.queue.entry_ids().len();
        assert_eq!(pane_len, 1, "pane must drop the send-now echo too");
        assert_eq!(
            agent.held_queue_count(),
            pane_len,
            "hint and pane share one visible-held filter"
        );
        assert_eq!(
            agent.visible_held_queue_len(),
            pane_len,
            "ungated visible length matches pane rows"
        );
        assert!(agent.held_queue_top_sendable());
        assert_eq!(
            format!(" · {} queued, Enter to send now", agent.held_queue_count()),
            " · 1 queued, Enter to send now"
        );

        agent
            .shared_queue
            .push(crate::app::prompt_queue::QueueEntryWire {
                id: "held-2".into(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "second held".into(),
                position: 2,
                combined_texts: None,
            });
        assert_eq!(agent.held_queue_count(), 2);
        agent.sync_queue_pane();
        let pane_len = agent.queue.entry_ids().len();
        assert_eq!(pane_len, 2);
        assert_eq!(agent.held_queue_count(), pane_len);
        assert_eq!(
            format!(" · {} queued, Enter to send now", agent.held_queue_count()),
            " · 2 queued, Enter to send now"
        );
    }

    #[test]
    fn has_held_user_queue_includes_armed_send_now_echo() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.pending_prompts.clear();
        agent.shared_queue.clear();
        assert!(
            !agent.has_held_user_queue(),
            "empty held occupancy while parked"
        );

        agent.expect_send_now_cancel = Some("send-now-echo".into());
        agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
            id: "send-now-echo".into(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "send now payload".into(),
            position: 0,
            combined_texts: None,
        }];
        assert!(
            agent.has_held_user_queue(),
            "armed send-now occupies hold even when pane-visible count is 0"
        );
        assert_eq!(
            agent.visible_held_queue_len(),
            0,
            "pane still hides the echo"
        );
        assert_eq!(agent.held_queue_count(), 0);
    }

    /// An arm that became the running turn is not held occupancy.
    /// Otherwise a new prompt is wrongly held behind an empty queue after a send-now adopts.
    #[test]
    fn has_held_user_queue_excludes_arm_that_is_running() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.pending_prompts.clear();
        agent.shared_queue.clear();

        agent.expect_send_now_cancel = Some("p-run".into());
        agent.session.current_prompt_id = Some("p-run".into());
        assert!(
            !agent.has_held_user_queue(),
            "an arm for the running turn is not held occupancy"
        );

        agent.session.current_prompt_id = Some("p-other".into());
        assert!(
            agent.has_held_user_queue(),
            "an arm for a non-running prompt still occupies hold"
        );
    }

    #[test]
    fn local_delete_of_last_held_row_adds_no_marker() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        enqueue_local(&mut app, id, "held row");
        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

        let agent = app.agents.get_mut(&id).unwrap();
        assert_eq!(count_turn_markers(agent), 0);
        assert!(
            agent.renders_parked(),
            "parked look on even with a held row"
        );

        // Delete the row through the queue-pane key path.
        agent.queue.sync_from_merged(
            &agent.session.pending_prompts,
            &agent.shared_queue,
            agent.session.current_prompt_id.as_deref(),
            agent.expect_send_now_cancel.as_deref(),
            &agent.send_now_painted_blocks,
        );
        let ids = agent.queue.entry_ids();
        assert_eq!(ids.len(), 1);
        let Some(&qid) = ids.first() else {
            panic!("expected queue id: {ids:?}");
        };
        agent.queue.list_state.select_by_id(qid);
        let registry = crate::actions::ActionRegistry::defaults();
        let _ = agent.handle_queue_key(
            &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &registry,
        );

        assert!(agent.session.pending_prompts.is_empty());
        assert_eq!(
            count_turn_markers(agent),
            0,
            "deleting the last held row must not write a marker"
        );
        assert!(agent.renders_parked(), "the stopped look stays on");
    }

    /// A task-tool refinement that omits `run_in_background` means background (the shell's serde default is true).
    /// The provisional foreground Subagent wait must clear, not stick as "Waiting on subagent…".
    #[test]
    fn task_refinement_without_background_field_defaults_to_background() {
        use std::sync::Arc;

        use crate::acp::tracker::{TurnActivity, WaitingReason};

        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);
        let agent = app.agents.get_mut(&id).unwrap();

        let meta = crate::acp::meta::NotificationMeta::default();
        // Meta-less task ToolCall (old shell): provisional Subagent wait.
        agent.session.handle_update(
            agent_client_protocol::SessionUpdate::ToolCall(
                agent_client_protocol::ToolCall::new(
                    agent_client_protocol::ToolCallId::new(Arc::from("task-old-shell")),
                    "task",
                )
                .kind(agent_client_protocol::ToolKind::Other)
                .status(agent_client_protocol::ToolCallStatus::Pending)
                .content(vec![])
                .locations(vec![]),
            ),
            &meta,
            &mut agent.scrollback,
        );
        assert!(
            matches!(
                agent.resolve_turn_activity(),
                Some(TurnActivity::Waiting(WaitingReason::Subagent { .. }))
            ),
            "the meta-less spawn starts with a provisional foreground wait"
        );
        // Refinement with raw_input that omits run_in_background/background.
        agent.session.handle_update(
            agent_client_protocol::SessionUpdate::ToolCallUpdate(
                agent_client_protocol::ToolCallUpdate::new(
                    agent_client_protocol::ToolCallId::new(Arc::from("task-old-shell")),
                    agent_client_protocol::ToolCallUpdateFields::new().raw_input(Some(
                        serde_json::json!({
                            "variant": "Task",
                            "task_id": "t-1",
                            "prompt": "explore"
                        }),
                    )),
                ),
            ),
            &meta,
            &mut agent.scrollback,
        );

        assert!(
            !matches!(
                agent.resolve_turn_activity(),
                Some(TurnActivity::Waiting(WaitingReason::Subagent { .. }))
            ),
            "an omitted run_in_background field means background: the provisional \
             foreground wait must clear"
        );
    }

    /// A plain mid-turn interjection needs no suppression state for a later park in the same turn.
    #[test]
    fn interjection_then_later_park_still_renders_parked() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);

        let _ = dispatch(
            Action::Interject {
                text: "heads up".into(),
                images: Vec::new(),
            },
            &mut app,
        );
        assert!(
            !test_agent(&app, id).renders_parked(),
            "no wait → no parked look"
        );

        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");
        let agent = app.agents.get_mut(&id).unwrap();
        assert!(agent.renders_parked(), "later park renders parked");
        assert_eq!(count_turn_markers(agent), 0, "and stays markerless");
    }

    /// Parked chrome must clear OSC 9;4 (and treat the tab title as idle).
    /// Ghostty/WezTerm then drop the progress bar while the session looks stopped.
    #[test]
    fn parked_wait_clears_progress_bar_notification() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        dispatch(Action::SendPrompt("first".into()), &mut app);

        app.update_notifications();
        assert!(
            app.notification_service.is_progress_active(),
            "live turn must keep the OSC 9;4 progress indicator active"
        );

        simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");
        let agent = app.agents.get_mut(&id).unwrap();
        assert!(agent.renders_parked());
        assert!(
            agent.session.state.is_busy(),
            "server-side turn remains running while parked"
        );

        app.pending_notification_escapes = None;
        app.update_notifications();
        assert!(
            !app.notification_service.is_progress_active(),
            "parked look must clear OSC 9;4 so the terminal progress bar stops"
        );
    }
}
