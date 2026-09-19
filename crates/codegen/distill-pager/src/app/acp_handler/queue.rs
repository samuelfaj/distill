use super::*;

/// A server-authoritative running prompt that drained into the running slot while the previous turn was still finishing locally (FIFO handoff race).
/// Stashed on [`AppView::pending_running_adoptions`] and consumed by the `PromptResponse` handler after `finish_turn` clears `current_prompt_id`.
#[derive(Debug, Clone)]
pub(crate) struct PendingRunningAdoption {
    /// The `prompt_id` the leader reported as `running_prompt_id`.
    pub prompt_id: String,
    /// The queued prompt's text (for the turn-start shim's user block), if the pager knew about the prompt.
    /// `None` for prompts queued by other clients.
    pub text: Option<String>,
    /// Display segments of a combined turn (always at least two); the shim paints one bubble per segment.
    pub combined_texts: Option<Vec<String>>,
    /// The adopted entry's `kind` (`"prompt"`, `"bash"`, `"verification"`, …), which selects the turn-start shim's display block and focus flag.
    pub kind: String,
    /// Set when a `running=None` broadcast spares this stash (one-shot: the next `running=None` tears it down).
    pub turn_ended: bool,
}

/// Wire payload of `x.ai/session/prompt_complete`, emitted by `MvpAgent::prompt()` on the shell after every turn.
/// `Serialize` is derived so tests construct payloads through the same type they are parsed into (shape drift fails at compile time, not at runtime).
/// Every field except `sessionId` is optional for wire compatibility with older shells; `promptId` only exists on shells with the lost-response fix.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PromptCompletePayload {
    pub(super) session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent_result: Option<String>,
    /// What triggered a cancelled turn's cancel (`"send_now"` suppresses the "Turn cancelled" marker); stamped top-level, absent on older shells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cancel_trigger: Option<String>,
    /// Why a cancelled turn was cancelled (`"HookDenied"` picks the blocked-by-hook marker); stamped top-level, absent on older shells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cancellation_category: Option<String>,
    /// Structured detail of a hook-denied cancel (hook name, reason) for the blocked-prompt card; stamped top-level, absent on older shells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cancellation_context: Option<serde_json::Value>,
    /// Typed kind of a failed stop; stamped top-level, absent on older shells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error_kind: Option<String>,
    /// `_meta` extension point, parsed defensively as a trigger fallback.
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub(super) meta: Option<serde_json::Value>,
}

impl PromptCompletePayload {
    /// The cancel trigger: the top-level `cancelTrigger` field, falling back to `_meta.cancelTrigger` (the durable rail's envelope shape).
    /// `None` (older shells) means a normal cancel.
    pub(super) fn cancel_trigger(&self) -> Option<&str> {
        self.cancel_trigger.as_deref().or_else(|| {
            self.meta
                .as_ref()?
                .get(super::super::turn_completion::CANCEL_TRIGGER_KEY)?
                .as_str()
        })
    }

    /// The cancellation category (`"HookDenied"` picks the blocked-by-hook marker): the top-level field, with the `_meta` envelope shape as fallback.
    /// `None` (older shells or plain user cancels) keeps the user-cancel copy.
    pub(super) fn cancellation_category(&self) -> Option<&str> {
        self.cancellation_category.as_deref().or_else(|| {
            self.meta
                .as_ref()?
                .get(super::super::turn_completion::CANCELLATION_CATEGORY_KEY)?
                .as_str()
        })
    }

    /// The hook-denied detail (hook name, reason): top-level with `_meta` fallback, like the category.
    /// `None` on older shells or non-hook cancels.
    pub(super) fn cancellation_context(&self) -> Option<&serde_json::Value> {
        self.cancellation_context.as_ref().or_else(|| {
            self.meta
                .as_ref()?
                .get(super::super::turn_completion::CANCELLATION_CONTEXT_KEY)
        })
    }

    /// Typed failure kind, parsed at this wire ingress.
    /// Absent parses to `None` (text recovery stays eligible); a present kind, known or unknown, is never text-sniffed (see `wire_error_kind`).
    pub(super) fn error_kind(&self) -> Option<crate::app::error_display::WireErrorType> {
        crate::app::error_display::wire_error_kind(self.error_kind.as_deref())
    }
}

/// Decided on the raw broadcast (before it moves into the queue mirror): listing the awaited prompt, queued or running, proves the shell holds it.
fn broadcast_acks_watch(
    view: Option<&AgentView>,
    changed: &crate::app::prompt_queue::QueueChanged,
) -> bool {
    view.and_then(|v| v.prompt_ack.as_ref())
        .is_some_and(|watch| crate::app::prompt_ack::queue_changed_acks(changed, watch.prompt_id()))
}

pub(super) fn handle_queue_changed(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(changed) =
        serde_json::from_str::<crate::app::prompt_queue::QueueChanged>(notif.params.get())
    else {
        tracing::warn!("Failed to parse x.ai/queue/changed");
        return false;
    };

    let running_prompt_id = changed.running_prompt_id.clone();
    let session_id = changed.session_id.clone();

    let sid = acp::SessionId::new(session_id.clone());
    let session_match = find_session_match(app, &sid);
    if let Some(SessionMatch::Child(parent_id)) = session_match {
        let acks_watch = broadcast_acks_watch(
            app.agents
                .get(&parent_id)
                .and_then(|parent| parent.subagent_views.get(&session_id))
                .map(|child| &**child),
            &changed,
        );
        let snapshot = changed.entries;
        if snapshot.is_empty() {
            app.shared_prompt_queues.remove(&session_id);
        } else {
            app.shared_prompt_queues
                .insert(session_id.clone(), snapshot.clone());
        }

        // Child queues are read-only mirrors; the root reconciliation and turn handling below must not run for them
        let is_active_parent = is_matched_agent_active(app, parent_id);
        let Some(parent) = app.agents.get_mut(&parent_id) else {
            return false;
        };
        let is_active = is_active_parent && parent.active_subagent.as_deref() == Some(&session_id);
        let Some(child) = parent.subagent_views.get_mut(&session_id) else {
            return false;
        };
        child.shared_queue = snapshot;
        child.sync_queue_pane();
        if acks_watch {
            child.note_prompt_ack(
                crate::app::prompt_ack::AckSignal::QueueChanged,
                std::time::Instant::now(),
            );
        }
        return is_active;
    }

    // Prefer running_* fields on the payload (authoritative; present when a turn is promoting)
    // Fall back to the local mirror for older shells
    let running_entry = running_prompt_id.as_ref().and_then(|pid| {
        app.shared_prompt_queue(&session_id)
            .and_then(|q| q.iter().find(|e| &e.id == pid).cloned())
    });
    let running_text: Option<String> = changed
        .running_text
        .clone()
        .or_else(|| running_entry.as_ref().map(|e| e.text.clone()));
    let running_combined: Option<Vec<String>> = changed
        .running_combined_texts
        .clone()
        .filter(|v| v.len() >= 2)
        .or_else(|| {
            running_entry
                .as_ref()
                .and_then(|e| e.combined_texts.clone())
                .filter(|v| v.len() >= 2)
        });
    let running_kind: String = changed
        .running_kind
        .clone()
        .or_else(|| running_entry.as_ref().map(|e| e.kind.clone()))
        .unwrap_or_else(|| "prompt".to_string());

    let agent_id = match session_match {
        Some(SessionMatch::Root(id)) => Some(id),
        _ => None,
    };

    let recv_entry_ids: Vec<&str> = changed.entries.iter().map(|e| e.id.as_str()).collect();
    // Raw (pre-merge) broadcast rows for the optimistic-echo reconcile
    // The post-apply snapshot re-pins unconfirmed echoes, so only the broadcast itself can prove a row landed shell-side
    let raw_entries: Vec<(String, u64)> = changed
        .entries
        .iter()
        .map(|e| (e.id.clone(), e.version))
        .collect();
    let local_current_prompt_id = agent_id
        .and_then(|aid| app.agents.get(&aid))
        .and_then(|a| a.session.current_prompt_id.clone())
        .unwrap_or_default();
    tracing::debug!(
        target: "qtrace",
        pid = std::process::id(),
        event = "queue_changed_recv",
        session = %session_id,
        running_prompt_id = running_prompt_id.as_deref().unwrap_or(""),
        local_current_prompt_id = %local_current_prompt_id,
        entry_count = changed.entries.len(),
        entries = ?recv_entry_ids,
        "received x.ai/queue/changed broadcast",
    );

    let acks_watch = broadcast_acks_watch(agent_id.and_then(|aid| app.agents.get(&aid)), &changed);
    let rekeyed_echo_ids = app.apply_queue_changed(changed);

    // Mirror the reconciled shared queue into the owning agent
    // The queue pane can then render the union of local and server rows without `AppView` access during draw or input handling
    if let Some(aid) = agent_id {
        let snapshot = app
            .shared_prompt_queue(&session_id)
            .cloned()
            .unwrap_or_default();
        // Stashed adoption: its painted block is about to be consumed.
        let stashed_pid = app
            .pending_running_adoptions
            .get(&aid)
            .map(|p| p.prompt_id.clone());
        if let Some(agent) = app.agents.get_mut(&aid) {
            agent.shared_queue = snapshot;
            if acks_watch {
                agent.note_prompt_ack(
                    crate::app::prompt_ack::AckSignal::QueueChanged,
                    std::time::Instant::now(),
                );
            }
            // A re-keyed echo's old id is dead everywhere; only its content matched the broadcast
            // Drop it from the optimistic set and any send-now parked on it
            // The row is visible under its new id, so a fresh Enter sends it normally
            for (old_id, new_id) in &rekeyed_echo_ids {
                agent.note_queue_echo_rekeyed(old_id, new_id);
            }

            // A painted-pending prompt the broadcast no longer lists was removed and will never adopt: retire its block
            // The running prompt and a stashed adoption are not removals
            // Unconfirmed optimistic ids are exempt (their RPC is in flight; absence is expected)
            let removed_painted: Vec<String> = agent
                .send_now_painted_blocks
                .keys()
                .filter(|pid| {
                    running_prompt_id.as_deref() != Some(pid.as_str())
                        && stashed_pid.as_deref() != Some(pid.as_str())
                        && !raw_entries.iter().any(|(eid, _)| eid == *pid)
                        && !agent.optimistic_queue_ids.contains(*pid)
                        // A Send Now on the active goal awaits its interjection claim
                        // Its row legitimately vanishes the instant the shell converts the Send Now into an interjection
                        // Keep it in place for `handle_interjection` to convert; retiring it would drop and re-push it at the end
                        && !agent.is_send_now_awaiting_interjection_claim(pid)
                })
                .cloned()
                .collect();
            for pid in &removed_painted {
                agent.retire_send_now_painted_block(pid);
            }

            // The user may be editing a server-origin row the broadcast no longer lists (started draining, removed by another client, etc.)
            // Exit editing mode so the composer isn't stranded on a ghost row
            // Don't dispatch any follow-up Action; the broadcast already reconciled the queue state for every other client
            let stranded_server_id = match &agent.prompt_mode {
                super::super::agent_view::PromptMode::EditingQueued {
                    server_id: Some(sid),
                    ..
                } if !agent.shared_queue.iter().any(|e| &e.id == sid) => Some(sid.clone()),
                _ => None,
            };
            if let Some(sid) = stranded_server_id {
                tracing::debug!(
                    server_id = %sid,
                    "exiting EditingQueued: row is no longer in the shared queue"
                );
                agent.cancel_editing_queued_for_lost_row();
            }
        }
        // Resolve a queue-row send-now that was parked while its row was still an optimistic echo
        // The broadcast just confirmed the row, so fire the interject with the authoritative version
        // Racing it earlier would have no-opped shell-side and dropped the send-now
        let fire = app.agents.get_mut(&aid).and_then(|agent| {
            agent.resolve_send_now_awaiting_confirm(&raw_entries, running_prompt_id.as_deref())
        });
        if let Some((id, expected_version)) = fire {
            if let Some(agent) = app.agents.get_mut(&aid) {
                // Same arming contract as `dispatch_queue_interject_shared`.
                super::super::dispatch::arm_send_now_and_paint(agent, &id, None);
            }
            crate::unified_log::info(
                "prompt.queue_send_now_confirmed",
                Some(&session_id),
                Some(serde_json::json!({ "prompt_id": id, "version": expected_version })),
            );
            app.pending_effects
                .push(crate::app::actions::Effect::QueueInterject {
                    session_id: sid.clone(),
                    id,
                    expected_version,
                    new_text: None,
                });
        }
    }

    // Wake-turn marker reconcile: `running_prompt_id` is authoritative
    // A broadcast naming a wake prompt lets the user stop it even before its first delta
    // Any other value retires a stale marker (recovery for a lost wake terminal)
    if let Some(aid) = agent_id
        && let Some(agent) = app.agents.get_mut(&aid)
    {
        let running = running_prompt_id.as_deref();
        if agent
            .running_wake_turn
            .as_ref()
            .is_some_and(|wake| Some(wake.prompt_id.as_str()) != running)
        {
            agent.running_wake_turn = None;
        }
        if let Some(pid) = running
            && is_wake_prompt(pid)
        {
            // The broadcast is authoritative: the shell says this wake is running, so an earlier terminal's record no longer applies
            agent.finished_wake_prompts.remove(pid);
            agent.note_streaming_wake_turn(pid);
        }
    }

    // Adoption and turn-start correlation
    // The single-client idle path stays inert: the pager already set `current_prompt_id` locally at `start_turn`
    // The confirming broadcast then arrives with `running_prompt_id == current_prompt_id` and the `Some(c) if c == pid` arm makes this a no-op
    match (running_prompt_id, agent_id) {
        // No turn running on the server: drop any stale pending adoption
        // Exception (`turn_ended`, one-shot): a turn ending inside the handoff window must leave the stash for the previous turn's PromptResponse
        // The stash stays whatever the update buffer holds
        (None, Some(aid)) => {
            let retain = app
                .pending_running_adoptions
                .get(&aid)
                .is_some_and(|p| !p.turn_ended);
            if retain {
                if let Some(p) = app.pending_running_adoptions.get_mut(&aid) {
                    p.turn_ended = true;
                }
            } else if let Some(p) = app.pending_running_adoptions.remove(&aid)
                && let Some(agent) = app.agents.get_mut(&aid)
            {
                agent.discard_pending_adoption_updates(&p.prompt_id);
            }
        }
        // One: an actor-run synthetic turn with no `prompt_complete` or `PromptResponse` exit, so nothing would ever call `finish_turn`
        // Adopting either via `apply_turn_start_shim` would call `start_turn()` and enter `AgentState::TurnRunning`
        // It must NOT re-adopt the turn the `SessionLoaded` or reconnect adoption already skipped
        (Some(pid), Some(aid))
            if app
                .agents
                .get(&aid)
                .is_some_and(|a| !a.should_adopt_running_prompt(&pid)) =>
        {
            tracing::debug!(
                target: "qtrace",
                pid = std::process::id(),
                prompt_id = %pid,
                "queue/changed: skipping turn-start adoption for non-adoptable running \
                 prompt (synthetic turn with no prompt_complete exit, or terminal-in-replay)",
            );
        }
        (Some(pid), Some(aid)) => {
            let current = app
                .agents
                .get(&aid)
                .and_then(|a| a.session.current_prompt_id.clone());
            match current {
                // Already tracking this running prompt; inert
                Some(c) if c == pid => {}
                // Nothing running locally: adopt now and run the turn-start shim (render the queued prompt's user block, set `TurnRunning`)
                None => {
                    let page_flip_entry = app.agents.get_mut(&aid).and_then(|agent| {
                        super::super::dispatch::apply_turn_start_shim(
                            agent,
                            pid,
                            running_text,
                            &running_kind,
                            running_combined,
                        )
                    });
                    super::super::dispatch::note_peek_page_flip(app, aid, page_flip_entry);
                }
                // A different prompt is still finishing locally: the FIFO handoff race
                // The next broadcast can arrive before the previous turn's `PromptResponse`
                // Never corrupt the in-flight turn
                Some(_) => {
                    // The leader emits this prompt's user-echo right after this broadcast (it has no `promptId`, so the gate can't drop it)
                    // Do that ONLY when THIS client will actually paint the block via the deferred shim
                    // That handler fires only for the client that DROVE the currently-finishing turn (`!attached_as_viewer`)
                    let drives_current_turn =
                        app.agents.get(&aid).is_some_and(|a| !a.attached_as_viewer);
                    let will_render_own_block = drives_current_turn
                        && super::super::dispatch::shim_renders_own_user_block(
                            &running_kind,
                            running_text.as_deref(),
                        );
                    if will_render_own_block && let Some(agent) = app.agents.get_mut(&aid) {
                        agent.session.tracker.expect_user_echo();
                    }
                    tracing::debug!(
                        target: "qtrace",
                        pid = std::process::id(),
                        event = "adoption_stashed",
                        prompt_id = %pid,
                        "stashing running-prompt adoption (FIFO handoff race)",
                    );
                    // Same-prompt rebroadcast must not clobber the stash (`text` is already None).
                    // Still fall through to local flush: promote may have just cleared the server front.
                    let same_stashed_prompt = app
                        .pending_running_adoptions
                        .get(&aid)
                        .is_some_and(|p| p.prompt_id == pid);
                    // A newer running prompt supersedes any earlier stash.
                    if !same_stashed_prompt
                        && let Some(prev) = app.pending_running_adoptions.insert(
                            aid,
                            PendingRunningAdoption {
                                prompt_id: pid.clone(),
                                text: running_text,
                                combined_texts: running_combined,
                                kind: running_kind,
                                turn_ended: false,
                            },
                        )
                        && let Some(agent) = app.agents.get_mut(&aid)
                    {
                        agent.discard_pending_adoption_updates(&prev.prompt_id);
                    }
                }
            }
        }
        _ => {}
    }
    // Retry local flush after server-queue rows clear (wait-start can arrive before promote).
    if let Some(aid) = agent_id
        && app
            .agents
            .get(&aid)
            .is_some_and(|agent| !agent.session.loading_replay)
    {
        let flush = crate::app::dispatch::flush_held_local_queue_into_wait(app, Some(aid));
        app.pending_effects.extend(flush);
    }
    true
}

/// `prompt_complete` carries `sessionId`, `stopReason`, `agentResult`, `turnId`, and (on shells with the lost-response fix) `promptId`.
/// For viewers, turns are serialized per session, so "finish the running viewer turn for this session" is unambiguous even without the prompt id.
/// TODO: prompt_complete-deprecation — the durable turn_completed is already consumed via finalize_turn_from_terminal.
pub(super) fn handle_prompt_complete(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(payload) = serde_json::from_str::<PromptCompletePayload>(notif.params.get()) else {
        tracing::warn!("Failed to parse x.ai/session/prompt_complete");
        return false;
    };
    let session_id = payload.session_id.as_str();

    let sid = acp::SessionId::new(session_id.to_string());
    let Some(SessionMatch::Root(id)) = find_session_match(app, &sid) else {
        return false;
    };
    let is_active = is_matched_agent_active(app, id);
    let Some(agent) = app.agents.get_mut(&id) else {
        return false;
    };

    // Finalize on the agent, then map the outcome to the return bool in the one shared place both terminal rails use
    // The outcome is returned directly; arming reports a change unconditionally so a background tab still wakes the reconcile tick
    let outcome = super::super::turn_completion::finalize_turn_from_terminal(
        agent,
        session_id,
        super::super::turn_completion::TerminalSignal {
            prompt_id: payload.prompt_id.as_deref(),
            stop_reason: payload.stop_reason.as_deref(),
            agent_result: payload.agent_result.as_deref(),
            cancel_trigger: payload.cancel_trigger(),
            cancellation_category: payload.cancellation_category(),
            cancellation_context: payload.cancellation_context(),
            error_kind: payload.error_kind(),
        },
    );
    super::super::turn_completion::apply_terminal_outcome(outcome, app, id, is_active)
}

#[cfg(test)]
mod tests {
    use super::PromptCompletePayload;

    /// The typed error kind parses from the wire `errorKind` field at this ingress.
    /// Absent parses to `None`; present but unknown parses to `Some(Other)`, so a newer shell's kind is never text-sniffed.
    #[test]
    fn prompt_complete_payload_error_kind_parses_at_ingress() {
        use crate::app::error_display::WireErrorType;

        let typed: PromptCompletePayload = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "stopReason": "error",
            "errorKind": "max_tokens_truncation",
        }))
        .expect("payload parses");
        assert_eq!(typed.error_kind(), Some(WireErrorType::MaxTokensTruncation));

        let unknown: PromptCompletePayload = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "stopReason": "error",
            "errorKind": "a_newer_shells_kind",
        }))
        .expect("payload parses");
        assert_eq!(unknown.error_kind(), Some(WireErrorType::Other));

        let absent: PromptCompletePayload = serde_json::from_value(serde_json::json!({
            "sessionId": "s1",
            "stopReason": "error",
        }))
        .expect("payload parses");
        assert_eq!(absent.error_kind(), None);
    }
}
