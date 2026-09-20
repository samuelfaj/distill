// Modified for Distill by Samuel Fajreldines, 2026.
//! Prompt and bash-command submission dispatchers and reload-window helpers.

use super::auth::{
    scrollback_has_recent_context_too_large, scrollback_has_recent_disk_full,
    scrollback_has_recent_reauth_prompt, scrollback_has_recent_request_failed,
};
use super::billing::is_credit_limit_error;
use super::ctx::{get_active_agent_mut, visible_agent_mut, with_active_agent};
use super::interject;
use super::queue::{
    apply_turn_start_shim, drain_prompt_state_to_last_queued, immediate_server_send_eligible,
    maybe_drain_queue, note_peek_page_flip, push_and_page_flip, push_server_queue_echo,
    retire_optimistic_echo,
};
use super::router::dispatch;
use super::turn::finish_turn_view;
use super::voice::{merge_prompt_with_voice_interim, voice_stop_on_submit};
use crate::app::actions::{Action, DoctorFixTarget, Effect};
use crate::app::agent::{AgentCommand, AgentId, AgentState};
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView};
use crate::app::cancel_latency::TurnEnd;
use crate::notifications::{NotificationEvent, NotificationEventKind};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::{MemoryCommandKind, SessionEvent};
use crate::slash::command::DoctorRequest;
use agent_client_protocol as acp;
use distill_telemetry::session_ctx::log_event;

/// Shared by every submit guard that refuses while the session reconnects.
pub(super) const RECONNECTING_NOTICE: &str = "Reconnecting, please wait...";

pub(super) use crate::app::agent_view::{
    BUILD_IN_FLIGHT_ABANDON_NOTICE, BUILD_IN_FLIGHT_REVISE_NOTICE, LEAVE_PLAN_REVISE_NOTICE,
};
pub(super) const LEAVE_PLAN_BUILD_NOTICE: &str =
    "Wait for plan mode to finish switching before building the plan.";

/// Chat kind for the next create: CLI `--chat` (`app.chat_mode`) or one-shot
/// `/chat` (`deferred_startup.pending_chat`, consumed here).
pub(super) fn consume_chat_kind(app: &mut AppView) -> bool {
    let pending = std::mem::take(&mut app.deferred_startup.pending_chat);
    app.chat_mode || pending
}

/// The prompt is always pushed to the queue first.
/// It does nothing special for auth or session lifecycle: it reuses the exact `NewSession` / `SendPrompt` actions the welcome screen dispatches.
/// `NewSession` is only dispatched when no session is active yet; a `--resume`/`-c`/`-w` session started earlier in startup is reused.
pub(crate) fn dispatch_initial_prompt(app: &mut AppView, prompt: String) -> Vec<Effect> {
    let mut effects = Vec::new();
    if matches!(app.active_view, ActiveView::Welcome) {
        // Same leave-home path as interactive send (Always worktree, draft swap).
        effects.extend(super::session::lifecycle::leave_welcome_for_session(app));
    } else if !matches!(app.active_view, ActiveView::Agent(_)) {
        effects.extend(dispatch(Action::NewSession, app));
    }
    if matches!(app.active_view, ActiveView::Welcome) {
        // Workspace ACK / create failed: replay after the gate, do not drop.
        app.deferred_startup.prompt = Some(prompt);
        return effects;
    }
    effects.extend(dispatch(Action::SendPrompt(prompt), app));
    effects
}

pub(super) fn collect_live_doctor_report_for_terminal(
    app: &AppView,
    agent_id: AgentId,
    terminal: &crate::terminal::TerminalContext,
) -> Option<crate::diagnostics::DiagnosticReport> {
    let agent = app.agents.get(&agent_id)?;
    let mut report = crate::slash::commands::doctor::DoctorCommand::report_for_terminal(
        terminal,
        app.screen_mode,
        crate::diagnostics::TuiRuntimeRequest {
            workspace: &agent.session.cwd,
            notification_method: app.notification_service.config().method,
            notification_protocol: app.notification_service.protocol(),
            notification_condition: app.notification_service.config().condition,
        },
    );
    if crate::app::voice_mode_enabled() {
        crate::diagnostics::apply_voice_probe(&mut report, true);
    }
    Some(report)
}

fn doctor_fix_target(agent: &AgentView) -> DoctorFixTarget {
    DoctorFixTarget {
        agent_id: agent.session.id,
        session_id: agent.session.session_id.clone(),
        session_binding_epoch: agent.session_binding_epoch,
        cwd: agent.session.cwd.clone(),
    }
}

pub(super) fn dispatch_doctor(request: DoctorRequest, app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(agent_id) = app.active_view else {
        return vec![];
    };
    let terminal = crate::terminal::terminal_context().clone();
    let Some(report) = collect_live_doctor_report_for_terminal(app, agent_id, &terminal) else {
        return vec![];
    };

    match request {
        DoctorRequest::Report => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.scrollback.push_block(RenderBlock::system(
                    crate::diagnostics::format_doctor(&report),
                ));
            }
        }
        DoctorRequest::ListFixes | DoctorRequest::Fix(_) => {
            let Some(agent) = app.agents.get(&agent_id) else {
                return vec![];
            };
            let target = doctor_fix_target(agent);
            return vec![Effect::PlanDoctorFix {
                target,
                report: Box::new(report),
                terminal,
                request,
            }];
        }
    }
    vec![]
}

pub(super) fn open_doctor_fix_question(
    app: &mut AppView,
    target: DoctorFixTarget,
    plan: Box<crate::diagnostics::FixPlan>,
) {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use distill_tools::implementations::distill::ask_user_question::{Question, QuestionOption};

    let Some(agent) = app.agents.get_mut(&target.agent_id) else {
        return;
    };
    if agent.question_view.is_some() {
        agent.scrollback.push_block(RenderBlock::system(
            "Close the current question before applying this fix.",
        ));
        return;
    }
    let preview = crate::diagnostics::format_fix_preview(&plan);
    let question = Question {
        question: "Apply this fix?".to_owned(),
        options: vec![
            QuestionOption {
                label: "Apply".to_owned(),
                description: "Make the changes shown above.".to_owned(),
                preview: Some(preview),
                id: None,
            },
            QuestionOption {
                label: "Cancel".to_owned(),
                description: "Do not change the configuration.".to_owned(),
                preview: None,
                id: None,
            },
        ],
        multi_select: Some(false),
        id: None,
    };
    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new("doctor-fix".to_owned(), vec![question], stashed)
        .with_local_kind(LocalQuestionKind::DoctorFix { target, plan })
        .with_no_freeform();
    agent.install_local_question(state);
    agent.prompt.set_text("");
}

pub(super) fn dispatch_execute_plan(
    app: &mut AppView,
    plan_file_content: String,
    plan_file_uri: Option<String>,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    // Same gate as send/bash: do not mark TurnRunning or fire ExecutePlan on a dead ACP channel.
    if app.reconnect_pending {
        agent.show_toast(RECONNECTING_NOTICE);
        return vec![];
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        agent.show_toast(super::ctx::NO_SESSION_NOTICE);
        return vec![];
    };
    if !agent.session.state.is_idle() {
        agent.show_toast("Wait for the current turn to end before building the plan.");
        return vec![];
    }
    // Shift+Tab / set_mode Off stages leave-Plan first. Approve must not
    // start ExecutePlan while that switch can still clear last_plan.
    if agent.plan_mode_pending == Some(false) {
        agent.show_toast(LEAVE_PLAN_BUILD_NOTICE);
        return vec![];
    }
    let prompt_id = uuid::Uuid::new_v4().to_string();
    // Stay mounted until the worker accepts. Local gates are not that
    // signal: a later refuse must not record approved or drop comments.
    agent.begin_local_turn(&prompt_id);
    // No local user row: `start_turn` armed echo-skip for a prompt we never pushed.
    // The daemon's execute-plan kickoff is the initiating message (same as cron/adopt).
    agent.session.tracker.clear_user_echo_skip();
    agent.set_execute_plan_prompt(prompt_id.clone());
    vec![Effect::ExecutePlan {
        agent_id: id,
        session_id,
        prompt_id,
        plan_file_content,
        plan_file_uri,
    }]
}

pub(super) fn dispatch_send_prompt(app: &mut AppView, text: String) -> Vec<Effect> {
    crate::unified_log::info(
        "prompt.enqueue",
        None,
        Some(serde_json::json!({"len": text.len()})),
    );
    dispatch_send_prompt_inner(
        app, text, /* consume_input */ true, /* literal */ false,
        /* is_follow_up */ false,
    )
}

/// Post-turn revise: keep approve/build open until the inner send is accepted.
/// Reconnect, a missing session, an in-flight build, and other inner aborts
/// must not toast "sent" or drop the review. Notes are literal so slash
/// commands and exit aliases go to the model instead of running as pager
/// commands (same as `SubmitFollowUp`).
pub(super) fn dispatch_revise_plan(app: &mut AppView, text: String) -> Vec<Effect> {
    if app.reconnect_pending {
        if let Some(agent) = get_active_agent_mut(app) {
            agent.show_toast(RECONNECTING_NOTICE);
        } else {
            app.show_toast(RECONNECTING_NOTICE);
        }
        return vec![];
    }
    let (has_post_turn_review, build_in_flight, leave_plan_pending) = match app.active_view {
        ActiveView::Agent(id) => app.agents.get(&id).map_or((false, false, false), |agent| {
            (
                agent
                    .plan_approval_view
                    .as_ref()
                    .is_some_and(|pav| pav.is_after_turn()),
                agent.execute_plan.is_some(),
                agent.plan_mode_pending == Some(false),
            )
        }),
        _ => (false, false, false),
    };
    if has_post_turn_review && leave_plan_pending {
        if let Some(agent) = get_active_agent_mut(app) {
            agent.show_toast(LEAVE_PLAN_REVISE_NOTICE);
        } else {
            app.show_toast(LEAVE_PLAN_REVISE_NOTICE);
        }
        return vec![];
    }
    if has_post_turn_review && build_in_flight {
        if let Some(agent) = get_active_agent_mut(app) {
            agent.show_toast(BUILD_IN_FLIGHT_REVISE_NOTICE);
        } else {
            app.show_toast(BUILD_IN_FLIGHT_REVISE_NOTICE);
        }
        return vec![];
    }
    let dispatched = dispatch_send_prompt_with_outcome(
        app, text, /* consume_input */ false, /* literal */ true,
        /* is_follow_up */ false,
    );
    if has_post_turn_review
        && dispatched.outcome.accepted_for_revise()
        && let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.commit_post_turn_plan_revised();
    }
    dispatched.effects
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptSendOutcome {
    None,
    Sent,
    Enqueued,
    Interjected,
}

impl PromptSendOutcome {
    fn accepted_for_revise(self) -> bool {
        matches!(self, Self::Sent | Self::Enqueued | Self::Interjected)
    }
}

struct PromptDispatchResult {
    effects: Vec<Effect>,
    outcome: PromptSendOutcome,
}

fn dispatch_send_prompt_with_outcome(
    app: &mut AppView,
    text: String,
    consume_input: bool,
    literal: bool,
    is_follow_up: bool,
) -> PromptDispatchResult {
    let queue_before = match app.active_view {
        ActiveView::Agent(id) => app
            .agents
            .get(&id)
            .map_or(0, |agent| agent.session.queue_len()),
        _ => 0,
    };
    let effects = dispatch_send_prompt_inner(app, text, consume_input, literal, is_follow_up);
    let outcome = classify_prompt_send(&effects, app, queue_before);
    PromptDispatchResult { effects, outcome }
}

fn classify_prompt_send(
    effects: &[Effect],
    app: &AppView,
    queue_before: usize,
) -> PromptSendOutcome {
    if effects
        .iter()
        .any(|effect| matches!(effect, Effect::SendInterject { .. }))
    {
        return PromptSendOutcome::Interjected;
    }
    if effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SendPrompt { .. } | Effect::SendPromptNow { .. }
        )
    }) {
        return PromptSendOutcome::Sent;
    }
    let Some(agent) = (match app.active_view {
        ActiveView::Agent(id) => app.agents.get(&id),
        _ => None,
    }) else {
        return PromptSendOutcome::None;
    };
    if agent.session.session_id.is_some()
        && agent.session.queue_len() > queue_before
        && agent
            .session
            .pending_prompts
            .back()
            .is_some_and(|queued| queued.kind == crate::app::agent::QueueEntryKind::Prompt)
    {
        return PromptSendOutcome::Enqueued;
    }
    PromptSendOutcome::None
}

/// Clear the active prompt into the stash (Esc Esc).
///
/// The draft goes to the stash, not the recall list. `Ctrl+S` is how it comes back.
pub(super) fn dispatch_clear_prompt(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        // Recoverable with the stash chord, but Esc-Esc is a discard: it never comes back on its own.
        agent.stash_prompt_draft(crate::app::agent_view::StashCause::ClearedDraft);
    });
    vec![]
}

/// Open the prompt-history search panel on the active agent (composer as the filter query).
/// Dispatched by `/history`; the slash pipeline has already cleared the composer, so the panel opens with an empty query.
pub(super) fn dispatch_open_history_search(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        let history = agent.combined_prompt_history();
        let current_text = agent.prompt.text().to_string();
        let opened = agent
            .prompt
            .history_search
            .activate(&history, &current_text);
        if !opened {
            // Matcher thread didn't start: the overlay stays closed on purpose (nothing could ever populate it)
            tracing::debug!("history search unavailable: matcher spawn failed");
        }
    });
    vec![]
}

/// Show the "ctrl+z to undo" hint after the user wiped a substantial draft.
/// Gated by the per-tip `contextual_hints.undo` gate (default ON).
/// The ephemeral-tip seen gate caps it at `UNDO_TIP_SEEN_CAP` shows per session (in-memory `app.tip_seen_counts`); nothing is persisted to disk.
pub(super) fn dispatch_show_undo_tip(app: &mut AppView) -> Vec<Effect> {
    if !app.contextual_hints.undo {
        return vec![];
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    // Shows and increments the per-session count in place (no disk write).
    // Emit the impression only when the tip actually took the slot (mirrors the `tip.shown` gate), so gated no-ops and TTL refreshes don't count
    if agent.show_ephemeral_tip(
        crate::tips::clear_detector::undo_tip(),
        &mut app.tip_seen_counts,
    ) {
        log_event(distill_telemetry::events::ContextualTip {
            tip: distill_telemetry::events::ContextualTipKind::Undo,
            action: distill_telemetry::events::ContextualTipAction::Shown,
        });
    }
    vec![]
}

/// Show the one-shot "Tight on space? Try /compact-mode" hint after the first stable agent-view draw landed in the small-screen band.
/// The trigger gates on the band and user compact OFF; see `AppView::maybe_trigger_small_screen_tip`.
/// Called directly from the draw-path trigger, not routed as an `Action`, so it returns `()` and "no effects from draw" holds structurally.
pub(in crate::app) fn show_small_screen_tip(app: &mut AppView) {
    if !app.contextual_hints.small_screen {
        return;
    }
    let ActiveView::Agent(id) = app.active_view else {
        return;
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return;
    };
    // Impression only when the tip actually takes the slot (mirrors undo/plan).
    if agent.show_ephemeral_tip(
        crate::tips::small_screen::small_screen_tip(),
        &mut app.tip_seen_counts,
    ) {
        log_event(distill_telemetry::events::ContextualTip {
            tip: distill_telemetry::events::ContextualTipKind::SmallScreen,
            action: distill_telemetry::events::ContextualTipAction::Shown,
        });
    }
}

/// Show the existing one-shot SSH discovery tip, redirected to `/doctor`.
pub(in crate::app) fn show_ssh_wrap_tip(app: &mut AppView) {
    if !app.contextual_hints.ssh_wrap {
        return;
    }
    let ActiveView::Agent(id) = app.active_view else {
        return;
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return;
    };
    if agent.show_ephemeral_tip(
        crate::tips::ssh_wrap::ssh_wrap_tip(),
        &mut app.tip_seen_counts,
    ) {
        log_event(distill_telemetry::events::ContextualTip {
            tip: distill_telemetry::events::ContextualTipKind::SshWrap,
            action: distill_telemetry::events::ContextualTipAction::Shown,
        });
    }
}

pub(super) fn dispatch_show_plan_nudge(app: &mut AppView) -> Vec<Effect> {
    if !app.contextual_hints.plan_mode {
        return vec![];
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    // Shows and increments the per-session count in place (no disk write).
    // Impression counts only on a real show (see `dispatch_show_undo_tip`).
    if agent.show_ephemeral_tip(
        crate::tips::plan_nudge::plan_nudge_tip(),
        &mut app.tip_seen_counts,
    ) {
        log_event(distill_telemetry::events::ContextualTip {
            tip: distill_telemetry::events::ContextualTipKind::PlanMode,
            action: distill_telemetry::events::ContextualTipAction::Shown,
        });
    }
    vec![]
}

/// After a fold/nav double-click on scrollback, tip that Word select lives in `/settings`. Gated by `contextual_hints.word_select` (default ON).
/// `/settings`. Gated by `contextual_hints.word_select` (default ON).
pub(super) fn dispatch_show_word_select_tip(app: &mut AppView) -> Vec<Effect> {
    if !app.contextual_hints.word_select {
        return vec![];
    }
    // Already on word_select: the tip would be wrong or redundant
    if crate::appearance::cache::load_keep_text_selection().selects_word() {
        return vec![];
    }
    let Some(agent) = visible_agent_mut(&mut app.agents, app.active_view) else {
        return vec![];
    };
    if agent.show_ephemeral_tip(
        crate::tips::word_select::word_select_tip(),
        &mut app.tip_seen_counts,
    ) {
        log_event(distill_telemetry::events::ContextualTip {
            tip: distill_telemetry::events::ContextualTipKind::WordSelect,
            action: distill_telemetry::events::ContextualTipAction::Shown,
        });
    }
    // Snapshot the prompt as of this double-click (also on a same-key TTL refresh: a new double-click is a new moment)
    // Any later divergence (typed, pasted, dropped) refuses the chord and retires the tip
    // A no-show gated by the seen cap leaves the slot to another tip and skips this
    if agent.ephemeral_tip.current_key() == Some(crate::tips::word_select::WORD_SELECT_TIP_KEY) {
        agent.word_select_tip_prompt_snapshot = Some(agent.prompt.text().to_string());
    }
    vec![]
}

/// Gate + telemetry + show for one view. Tick path is the only caller.
pub(in crate::app) fn present_export_copy_tip(
    agent: &mut AgentView,
    seen_counts: &mut std::collections::HashMap<&'static str, u32>,
    gate: bool,
) -> bool {
    if !gate {
        return false;
    }
    // Already on screen: that timer owns the slot (do not refresh TTL or re-count).
    if agent.ephemeral_tip.current_key() == Some(crate::tips::export_copy::EXPORT_COPY_TIP_KEY) {
        return false;
    }
    let shown = agent.show_ephemeral_tip(crate::tips::export_copy::export_copy_tip(), seen_counts);
    if shown {
        log_event(distill_telemetry::events::ContextualTip {
            tip: distill_telemetry::events::ContextualTipKind::ExportCopy,
            action: distill_telemetry::events::ContextualTipAction::Shown,
        });
    }
    shown
}

/// Accept the word-select tip via its advertised chord.
/// Retires the tip so one impression maps to at most one acceptance.
/// No-op unless the tip is on screen: the chord is tip-scoped and must not become a global setting toggle.
pub(super) fn dispatch_accept_word_select_tip(app: &mut AppView) -> Vec<Effect> {
    let Some(agent) = get_active_agent_mut(app) else {
        return vec![];
    };
    if agent.ephemeral_tip.current_key() != Some(crate::tips::word_select::WORD_SELECT_TIP_KEY) {
        return vec![];
    }
    agent
        .ephemeral_tip
        .clear(crate::tips::word_select::WORD_SELECT_TIP_KEY);
    agent.word_select_tip_prompt_snapshot = None;
    log_event(distill_telemetry::events::ContextualTip {
        tip: distill_telemetry::events::ContextualTipKind::WordSelect,
        action: distill_telemetry::events::ContextualTipAction::Accepted,
    });
    super::settings::setters::set_keep_text_selection(
        app,
        crate::appearance::TextSelection::WordSelect,
    )
}

/// Transient `Queued · Enter to send now` after a mid-turn queue. Skipped when the dock is active, since its Queued section surfaces the same queue; still fires when the dock is off.
/// the dock is active, since its Queued section surfaces the same queue; still
/// fires when the dock is off.
fn maybe_show_send_now_tip(app: &mut AppView) {
    if !app.contextual_hints.send_now {
        return;
    }
    let ActiveView::Agent(id) = app.active_view else {
        return;
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return;
    };
    // `dock_on` (dock enabled with room to paint), not `dock_shown`: the queue
    // we just added flips `dock_shown` true only on the next render, but the
    // dock will surface it, so the tip is redundant now.
    if agent.dock_on {
        return;
    }
    if agent.show_ephemeral_tip(
        crate::tips::send_now::send_now_tip(),
        &mut app.tip_seen_counts,
    ) {
        log_event(distill_telemetry::events::ContextualTip {
            tip: distill_telemetry::events::ContextualTipKind::SendNow,
            action: distill_telemetry::events::ContextualTipAction::Shown,
        });
    }
}

/// Body of [`dispatch_send_prompt`], parameterized over whether to consume the prompt textarea after the command is processed.
/// `consume_input = true` (Enter from the prompt) wipes the textarea, drains pending images into the queue, and inserts the text into up-arrow history.
/// The slash-command and exit-alias branches are skipped so server- or model-controlled chip text can never execute a command.
pub(super) fn dispatch_send_prompt_inner(
    app: &mut AppView,
    text: String,
    consume_input: bool,
    literal: bool,
    is_follow_up: bool,
) -> Vec<Effect> {
    dispatch_send_prompt_submission(app, text, None, consume_input, literal, is_follow_up)
}

pub(super) fn dispatch_send_prompt_submission(
    app: &mut AppView,
    text: String,
    submission: Option<crate::views::prompt_widget::StashedPrompt>,
    consume_input: bool,
    literal: bool,
    is_follow_up: bool,
) -> Vec<Effect> {
    // Submitting is a fresh intent that retires any pending double-press
    // The AppView pending-action check only resets on KEY events
    // A submit with no intervening key (mouse send, `SubmitFollowUp`, `RevisePlan`, `SendSlashCommandPreservingDraft`) would otherwise leave a stale pending action
    app.pending_action = None;

    if app.reconnect_pending {
        app.show_toast(RECONNECTING_NOTICE);
        return vec![];
    }

    let mut prelude = Vec::new();
    if matches!(app.active_view, ActiveView::Welcome) {
        prelude = super::session::lifecycle::leave_welcome_for_session(app);
    }

    let ActiveView::Agent(id) = app.active_view else {
        if !text.trim().is_empty() {
            app.deferred_startup.prompt = Some(text);
        }
        return prelude;
    };
    // Match the later slash path: only a line that itself starts with `/` is a command.
    // Chip-stripped text can look like `/feedback` after a leading image without being one.
    if !literal && text.trim().starts_with('/') {
        let slash_input = submission.as_ref().map_or_else(
            || {
                app.agents.get(&id).map_or_else(
                    || text.clone(),
                    |agent| agent.prompt.submitted_text_without_image_chips(&text),
                )
            },
            |submission| submission.text_without_image_chips(),
        );
        if let Some(invocation) = crate::slash::parse_invocation(slash_input.trim())
            && let Some(agent) = app.agents.get(&id)
            && let Some(command) = agent
                .prompt
                .slash_controller
                .registry()
                .get_for_dispatch(invocation.token)
        {
            let voice_owns_prompt = consume_input
                && app.voice_recording_target()
                    == Some(crate::app::app_view::VoiceTarget::Agent(id));
            if let Some(refusal) = command.submission_refusal(invocation.args, voice_owns_prompt) {
                if app.screen_mode.is_minimal() {
                    with_active_agent(app, |agent| {
                        agent
                            .scrollback
                            .push_block(RenderBlock::system(refusal.to_string()));
                    });
                } else {
                    app.show_toast(refusal);
                }
                return vec![];
            }
        }
    }

    // Promote the interim and hard-reset; merge only when consuming the composer.
    let interim = voice_stop_on_submit(app);
    let text = if consume_input {
        merge_prompt_with_voice_interim(text, interim)
    } else {
        text
    };

    // Capture app-level fields before the mut-borrow on `agent`.
    let coding_data_sharing_opt_out_from_app = app.coding_data_retention_opt_out;
    let coding_data_sharing_lock_from_app = app.coding_data_sharing_lock();
    let show_tips_from_app = app.show_tips;
    let auto_update_from_app = app.auto_update;
    let respect_manual_folds_from_app = app.appearance.scrollback.scroll.respect_manual_folds;
    let auto_mode_gate_from_app = app.auto_mode_gate;
    let ask_user_question_timeout_enabled_from_app = app.ask_user_question_timeout_enabled;
    let voice_stt_language_from_app = app.voice_config.language.clone();
    let login_method_id_from_app = app.login_method_id.as_ref().map(|id| id.0.to_string());
    let leader_mode = app.leader_mode;
    let screen_mode_is_minimal = app.screen_mode.is_minimal();
    let billing_surface_visible = app.grok_billing_surface_visible();
    let usage_command_visible = app.usage_command_visible();
    let Some(agent) = app.agents.get_mut(&id) else {
        return prelude;
    };

    // Paste-then-immediate-send: an image probe from a just-pasted Cmd+V is still off-thread
    // Stash this send and re-issue it once the probe completes so the image is never dropped from the built content blocks
    // Scoped to `consume_input` sends: only those clear the draft, so only they can drop a not-yet-attached image
    if consume_input && agent.paste_probe_in_flight > 0 {
        agent.deferred_send = Some(crate::app::agent_view::AgentDeferredSend::SendPrompt);
        return prelude;
    }

    // Submitting the prompt retires any edit-contextual ephemeral tip (ambient tips live out their TTL across the submit)
    agent.ephemeral_tip.clear_on_submit();

    let slash_input = submission.as_ref().map_or_else(
        || agent.prompt.submitted_text_without_image_chips(&text),
        |submission| submission.text_without_image_chips(),
    );
    // Raw text decides command-ness so a chip-stripped ` /btw q` still hoists.
    // `/goal` is checked on that typed line first; `/btw` hoist would bury it.
    let is_plain_submission = !literal && !text.trim().starts_with('/');
    if is_plain_submission
        && crate::slash::mid_text_hoist::contains_goal_command_token(
            &slash_input,
            agent.prompt.slash_controller.registry(),
        )
    {
        if screen_mode_is_minimal {
            agent.scrollback.push_block(RenderBlock::system(
                crate::slash::mid_text_hoist::MID_TEXT_GOAL_NOTICE.to_owned(),
            ));
        } else {
            agent.show_toast(crate::slash::mid_text_hoist::MID_TEXT_GOAL_NOTICE);
        }
        return prelude;
    }
    let hoisted = if is_plain_submission {
        crate::slash::mid_text_hoist::hoist_mid_text_command(
            &slash_input,
            agent.prompt.slash_controller.registry(),
        )
    } else {
        None
    };

    // Recorded before the registry runs, because most command outcomes return on their own path.
    let recorded_as_command =
        !literal && consume_input && (hoisted.is_some() || text.trim().starts_with('/'));
    if recorded_as_command {
        // Up-arrow history recalls the message as typed, not the hoisted `/command` rewrite.
        agent.record_prompt_in_history(text.trim());
    }
    // The args site re-parses `slash_input`, so the hoisted line has to replace it too.
    let (text, slash_input) = match hoisted {
        Some(hoisted) => (hoisted.clone(), hoisted),
        None => (text, slash_input),
    };
    let trimmed = text.trim();

    let mut effects = prelude;
    let mut tip_send_now_after_queue = false;

    // Tier restricted command upsell
    // A typed invocation would otherwise fall through the unknown-command path below and leak to the model as a raw prompt
    // Upsell instead; genuinely unknown commands still pass through (shell/ACP commands depend on that)
    if !literal
        && trimmed.starts_with('/')
        && let Some(invocation) = crate::slash::parse_invocation(trimmed)
        && agent
            .prompt
            .slash_controller
            .registry()
            .is_restricted(invocation.token)
    {
        // Only consume the composer when the upsell can actually open
        // With another question modal already up, `open_supergrok_upsell` would no-op and wiping the composer here would silently drop the typed text
        // Keep it instead so the user can resubmit after closing the modal, and never fall through to passthrough for restricted commands
        if agent.question_view.is_none() {
            if consume_input {
                agent.prompt.set_text("");
            }
            let opened =
                super::billing::open_restricted_command_upsell(agent, login_method_id_from_app);
            debug_assert!(opened, "no modal was open, so the upsell must open");
        }
        return effects;
    }

    // Registry based slash command execution
    // If the text starts with `/`, run it through the slash registry.
    // `literal` (chip click) skips this so chip text is never a command.
    if !literal && trimmed.starts_with('/') {
        use crate::slash::command::{CommandExecCtx, CommandResult};
        use crate::slash::parse_invocation;

        // Build execution context.
        let exec_result = {
            let mut ctx = CommandExecCtx {
                models: &agent.session.models,
                session_id: agent.session.session_id.as_ref(),
                bundle_state: &app.bundle_state,
                screen_mode: app.screen_mode,
                billing_surface_visible,
                usage_command_visible,
                // PAGER-owned snapshot for slash commands.
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: agent.multiline_mode,
                    yolo_mode: agent.session.is_yolo(),
                    auto_mode: agent.session.is_auto(),
                    current_model_name: agent.session.models.current_model_name(),
                    available_models: agent
                        .session
                        .models
                        .available
                        .iter()
                        .map(|(id, info)| (info.name.clone(), id.clone()))
                        .collect(),
                    coding_data_sharing_opt_out: coding_data_sharing_opt_out_from_app,
                    coding_data_sharing_lock: coding_data_sharing_lock_from_app,
                    // Prefer optimistic pending over confirmed active.
                    plan_mode_active: agent.plan_mode_pending.unwrap_or(agent.plan_mode_active),
                    show_tips: show_tips_from_app,
                    auto_update: auto_update_from_app,
                    vim_mode: crate::appearance::cache::load_vim_mode(),
                    scroll_speed: crate::appearance::cache::load_scroll_speed(),
                    respect_manual_folds: respect_manual_folds_from_app,
                    auto_mode_gate: auto_mode_gate_from_app,
                    ask_user_question_timeout_enabled: ask_user_question_timeout_enabled_from_app,
                    voice_stt_language: voice_stt_language_from_app,
                },
            };

            if let Some(invocation) = parse_invocation(trimmed) {
                let (is_builtin, command) = {
                    let reg = agent.prompt.slash_controller.registry();
                    let is_builtin = reg.is_builtin(invocation.token);
                    // Bypasses only the menu-only hide (hard gates still return `None`); see `CommandRegistry::get_for_dispatch`
                    let command = reg.get_for_dispatch(invocation.token).cloned();
                    (is_builtin, command)
                };
                {
                    use distill_telemetry::events::{PagerCommandSource, PagerSlashCommand};
                    use distill_telemetry::session_ctx::log_event;
                    let source = if is_builtin {
                        PagerCommandSource::Builtin
                    } else {
                        PagerCommandSource::NonBuiltin
                    };
                    log_event(PagerSlashCommand {
                        command_name: invocation.token.to_string(),
                        source,
                    });
                }
                if let Some(command) = command {
                    // Central screen-mode gate
                    // A fully-typed invocation thus earns a hint that names the way out instead of leaking to the model
                    // A refusal added here must also extend the pre-check in `EditedCommandGate` (`dispatch::queue`)
                    if let Some(refusal) = command
                        .mode_support()
                        .refusal(invocation.token, ctx.screen_mode)
                    {
                        CommandResult::Message(refusal)
                    } else {
                        agent
                            .prompt
                            .slash_controller
                            .record_command_use(invocation.token, invocation.token);
                        let args = parse_invocation(slash_input.trim())
                            .map_or(invocation.args, |invocation| invocation.args);
                        command.run(&mut ctx, args)
                    }
                } else {
                    // Unknown command: pass through to shell
                    CommandResult::PassThrough(text.clone())
                }
            } else {
                // Bare `/` or malformed: pass through
                CommandResult::PassThrough(text.clone())
            }
        };

        // Map CommandResult to pager behavior. (MRU persistence is queued off-thread inside `record_command_use` above.)
        match exec_result {
            CommandResult::Handled => {
                if consume_input {
                    agent.prompt.set_text("");
                }
                return effects;
            }
            CommandResult::Error(msg) => {
                if consume_input {
                    agent.prompt.set_text("");
                }
                push_and_page_flip(&mut agent.scrollback, RenderBlock::system(msg));
                return effects;
            }
            CommandResult::Message(msg) => {
                if consume_input {
                    agent.prompt.set_text("");
                }
                push_and_page_flip(&mut agent.scrollback, RenderBlock::system(msg));
                return effects;
            }
            CommandResult::Doctor(request) => {
                if consume_input {
                    agent.prompt.set_text("");
                }
                effects.extend(dispatch_doctor(request, app));
                return effects;
            }
            CommandResult::Action(Action::ExitSession) => {
                if consume_input {
                    agent.prompt.set_text("");
                }
                effects.extend(dispatch(Action::ExitSession, app));
                return effects;
            }
            CommandResult::Action(Action::EditPromptExternal) => {
                // Typed slash input occupies the composer; the palette route preserves an existing draft.
                if consume_input {
                    agent.prompt.set_text("");
                }
                effects.extend(dispatch(Action::EditPromptExternal, app));
                return effects;
            }
            CommandResult::Action(Action::SendRememberNote(note)) => {
                if consume_input {
                    agent.prompt.set_text("");
                }
                // The typed `/remember <text>` is the row already recorded above.
                effects.extend(super::notes::dispatch_send_remember_note_from_command(
                    app, note,
                ));
                return effects;
            }
            CommandResult::Action(Action::OpenFeedbackModal(mut open)) => {
                // Composer chips stay put until this open is accepted. A no-session
                // or blocker refusal drops `open`, and FeedbackImages Drop would
                // unlink any drained files. An already-open modal is also a refusal.
                let prior_modal_id = agent.feedback_modal.as_ref().map(|modal| modal.id());
                open.images = submission
                    .map(|submission| submission.into_submission().1)
                    .unwrap_or_default()
                    .into();
                let open_effects = dispatch(Action::OpenFeedbackModal(open), app);
                let Some(agent) = app.agents.get_mut(&id) else {
                    effects.extend(open_effects);
                    return effects;
                };
                let accepted = agent
                    .feedback_modal
                    .as_ref()
                    .is_some_and(|modal| Some(modal.id()) != prior_modal_id);
                let mut rehydrate = Vec::new();
                if accepted && consume_input {
                    let drained = agent.prompt.drain_images();
                    if !drained.is_empty()
                        && let Some(modal) = agent.feedback_modal.as_mut()
                    {
                        let modal_id = modal.id();
                        for (image_identity, path) in modal.absorb_composer_images(drained) {
                            rehydrate.push(Effect::RehydrateFeedbackImage {
                                agent_id: id,
                                modal_id,
                                image_identity,
                                path,
                            });
                        }
                    }
                    agent.prompt.set_text("");
                }
                effects.extend(open_effects);
                effects.extend(rehydrate);
                return effects;
            }
            CommandResult::Action(mut action) => {
                let mut submitted_images = submission
                    .map(|submission| submission.into_submission().1)
                    .unwrap_or_default();
                if consume_input {
                    submitted_images.extend(agent.prompt.drain_images());
                }
                match &mut action {
                    Action::SendFeedback { images, .. } => {
                        *images = submitted_images.into();
                    }
                    // Same as feedback: these images are the question, not leftover chips.
                    Action::SendBtw { images, .. } => {
                        *images = submitted_images;
                    }
                    _ => crate::prompt_images::drain_and_cleanup(
                        crate::prompt_images::SessionPathPolicy::Preserve,
                        &mut submitted_images,
                    ),
                }
                if consume_input {
                    agent.prompt.set_text("");
                }
                effects.extend(dispatch(action, app));
                return effects;
            }
            CommandResult::QueueCommand(cmd_text) => {
                agent.session.enqueue_command(cmd_text);
            }
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                display_as_skill,
                scheduled_task_preview,
            } => {
                // Enqueue with display text for scrollback but wire_blocks for the actual prompt sent to the model
                // Leading skill invocation: display_as_skill owns styling (no ranges)
                let id = agent.session.next_queue_id;
                agent.session.next_queue_id += 1;
                agent
                    .session
                    .pending_prompts
                    .push_back(crate::app::agent::QueuedPrompt {
                        wire_blocks: Some(prompt_blocks),
                        display_as_skill,
                        ..crate::app::agent::QueuedPrompt::plain(
                            id,
                            display_text,
                            crate::app::agent::QueueEntryKind::Prompt,
                        )
                    });

                // Insert a provisional scheduled task so the tasks pane shows it immediately, before the LLM round-trips through scheduler_create
                // Keyed by a provisional ID; replaced when the real ScheduledTaskCreated notification arrives
                if let Some(preview) = scheduled_task_preview {
                    use crate::app::agent::ScheduledTaskInfo;
                    let provisional_id = format!("provisional-{}", id);
                    agent.session.scheduled_tasks.insert(
                        provisional_id.clone(),
                        ScheduledTaskInfo {
                            task_id: provisional_id,
                            prompt: preview.prompt,
                            human_schedule: preview.human_schedule,
                            created_at: std::time::Instant::now(),
                            next_fire_at: preview.next_fire_at,
                            tag: preview.tag,
                            last_subagent_id: None,
                        },
                    );
                }
            }
            CommandResult::PassThrough(pass_text) => {
                // A recognized token later in the passthrough text still styles the echo.
                let skill_token_ranges = agent
                    .prompt
                    .slash_controller
                    .recognized_token_ranges(&pass_text, &agent.session.models);
                agent
                    .session
                    .enqueue_prompt_with_skill_tokens(pass_text, skill_token_ranges);
            }
        }
        // Reaching here means the command queued or passed text through, a real submission
        // Local-UI commands returned above and must keep the hook-block hold
        agent.credit_limit_stashed_prompt = None;
        agent.release_hook_block_hold();
        if consume_input {
            // Drain prompt images before clearing prompt state.
            drain_prompt_state_to_last_queued(agent);
            agent.prompt.set_text("");
            agent.note_draft_consumed();
        }
    } else if !literal && crate::slash::commands::exit::is_exit_alias(trimmed) {
        if consume_input {
            agent.prompt.set_text("");
        }
        effects.extend(dispatch(Action::Quit, app));
        return effects;
    } else {
        // Server-authoritative immediate send (plain prompt only)
        // The agent appends it to its authoritative `pending_inputs` (turn starts never overlap) and drives the drain via `x.ai/queue/changed`
        // So the chips are cleared ONLY when the suggestion actually sends or enqueues
        agent.release_hook_block_hold();
        if is_follow_up && agent.session.session_id.is_some() {
            agent.clear_follow_ups();
        }

        // Composer-recognized slash tokens at submit time: they style the scrollback echo and travel in the wire meta so replay restyles it
        let skill_token_ranges = agent
            .prompt
            .slash_controller
            .recognized_token_ranges(&text, &agent.session.models);

        let immediate_server_send =
            immediate_server_send_eligible(agent, leader_mode) && agent.prompt.images.is_empty();
        tracing::debug!(
            target: "qtrace",
            pid = std::process::id(),
            event = "send_route_plain",
            immediate = immediate_server_send,
            is_turn_running = agent.session.state.is_turn_running(),
            shared_queue_len = agent.shared_queue.len(),
            pending_len = agent.session.pending_prompts.len(),
            current_prompt_id = agent.session.current_prompt_id.as_deref().unwrap_or(""),
            session = agent.session.session_id.as_ref().map(|s| s.0.as_ref()).unwrap_or(""),
            images = agent.prompt.images.len(),
            text = %text.chars().take(48).collect::<String>(),
            "plain prompt send routing decision",
        );

        // Occupancy/park flags: only the image branch below does an immediate Send Now on an empty held wait
        let parked_sendable_wait = agent.is_parked_on_sendable_wait();
        let hold_behind_existing_queue = parked_sendable_wait && agent.has_held_user_queue();
        let queued_while_running = agent.session.state.is_turn_running();

        // Images can't use immediate server-send; a park on an empty held wait still does a Send Now
        if !immediate_server_send
            && immediate_server_send_eligible(agent, leader_mode)
            && !agent.prompt.images.is_empty()
            && parked_sendable_wait
            && !hold_behind_existing_queue
        {
            let images = agent.prompt.drain_images();
            if consume_input {
                agent.prompt.set_text("");
                agent.note_draft_consumed();
            }
            // A new prompt is taking over (same contract as the immediate-send branch below)
            agent.clear_follow_ups();
            effects.extend(interject::dispatch_send_prompt_now(app, text, images));
            return effects;
        }

        if immediate_server_send {
            let session_id = agent
                .session
                .session_id
                .clone()
                .expect("session_id is_some checked");
            let agent_id = agent.session.id;
            let prompt_id = uuid::Uuid::new_v4().to_string();
            // Self-originated: when this prompt becomes the running turn, the ACP gate must treat its deltas as ours, not another client's
            // Adoption happens via the `running_prompt_id` broadcast and the turn-start shim
            agent.note_self_originated_prompt(&prompt_id);
            // Plain image-free sends set no send-now cancel expectation: shell queue state and cancelTrigger decide the outcome

            if consume_input {
                // Plain prompt: no images to drain
                // Clear the textarea and record up-arrow history (same as the local path's history insert)
                agent.prompt.set_text("");
                agent.note_draft_consumed();
                agent.record_prompt_in_history(&text);
            }

            // A new prompt is taking over: the previous response's follow-up chips must not linger into it
            // This immediate-send path returns early, so it must clear them here too (notably a chip click, which submits while a turn is running)
            // `clear_follow_ups` keeps `follow_up_seen` (it marks the turn boundary) so a stale re-delivery stays rejected
            agent.clear_follow_ups();
            agent.credit_limit_stashed_prompt = None;

            // `agent` borrow ends here; push the optimistic echo via `app`.
            let sid_str = session_id.0.to_string();
            push_server_queue_echo(app, agent_id, &sid_str, &prompt_id, &text, "prompt");
            crate::unified_log::info(
                "prompt.send_server_authoritative",
                Some(&sid_str),
                Some(serde_json::json!({ "kind": "prompt", "len": text.len() })),
            );
            if queued_while_running
                && !parked_sendable_wait
                && !crate::appearance::cache::load_follow_up_steer()
            {
                maybe_show_send_now_tip(app);
            }
            effects.push(Effect::SendPrompt {
                agent_id,
                session_id,
                text,
                prompt_id,
                skill_token_ranges,
            });
            return effects;
        }

        agent
            .session
            .enqueue_prompt_with_skill_tokens(text.clone(), skill_token_ranges);
        agent.credit_limit_stashed_prompt = None;
        if consume_input {
            // Drain prompt images before clearing prompt state.
            drain_prompt_state_to_last_queued(agent);
            agent.prompt.set_text("");
            agent.note_draft_consumed();
        }
        tip_send_now_after_queue = queued_while_running;
    }

    if tip_send_now_after_queue {
        let inline_hint_shown = app
            .agents
            .get(&id)
            .is_some_and(|agent| agent.held_queue_count() > 0);
        if !inline_hint_shown {
            maybe_show_send_now_tip(app);
        }
    }

    let drain = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return effects;
        };

        // Skipped for modal-driven dispatch: the user didn't type these commands and shouldn't see them in up-arrow history.
        // `PassThrough`, `QueueCommand` and `InjectSkill` reach here, so a command recorded above would land twice.
        if consume_input && !recorded_as_command {
            agent.record_prompt_in_history(&text);
        }
        maybe_drain_queue(agent)
    };
    effects.extend(drain.effects);
    note_peek_page_flip(app, id, drain.page_flip_entry);
    // A prompt queued while the turn is already busy (wait, live watcher, running tool) would otherwise sit locally until the next ACP batch
    // An open /btw overlay does not produce that batch, so a send after `/btw` would stay queued for the rest of the wait
    // Release here, with the same helper the ACP re-check uses; a no-op when the turn is not busy
    effects.extend(super::queue::maybe_release_queued_prompt_into_turn(
        app, None,
    ));
    effects
}

/// Enqueue a bash command and try to drain immediately.
/// Bash commands go through the same enqueue/drain pipeline as normal prompts, just with `QueueEntryKind::BashCommand`. No scrollback block is pushed here; the execute block from the shell IS the visual entry.
pub(super) fn dispatch_send_bash_command(app: &mut AppView, command: String) -> Vec<Effect> {
    if app.reconnect_pending {
        app.show_toast(RECONNECTING_NOTICE);
        return vec![];
    }

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let leader_mode = app.leader_mode;
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    // Submitting a bash command retires any edit-contextual ephemeral tip.
    agent.ephemeral_tip.clear_on_submit();
    agent.release_hook_block_hold();

    agent.record_prompt_in_history(&crate::app::agent_view::prompt_history_text(
        &command,
        crate::app::agent_view::PromptInputMode::Bash,
    ));

    // Server-authoritative immediate send for bash while running
    // A bash command typed while a turn is RUNNING is sent to the agent immediately (it's already a `session/prompt` with bash meta)
    // It is echoed into the shared queue with `kind="bash"`
    let bash_immediate = immediate_server_send_eligible(agent, leader_mode);
    tracing::debug!(
        target: "qtrace",
        pid = std::process::id(),
        event = "send_route_bash",
        immediate = bash_immediate,
        is_turn_running = agent.session.state.is_turn_running(),
        shared_queue_len = agent.shared_queue.len(),
        pending_len = agent.session.pending_prompts.len(),
        current_prompt_id = agent.session.current_prompt_id.as_deref().unwrap_or(""),
        session = agent.session.session_id.as_ref().map(|s| s.0.as_ref()).unwrap_or(""),
        text = %command.chars().take(48).collect::<String>(),
        "bash command send routing decision",
    );
    if bash_immediate {
        let session_id = agent
            .session
            .session_id
            .clone()
            .expect("session_id is_some checked");
        let agent_id = agent.session.id;
        let prompt_id = uuid::Uuid::new_v4().to_string();
        // Self-originated (see the plain immediate-send path): keep this turn's deltas ours in the ACP gate once it becomes the running turn
        agent.note_self_originated_prompt(&prompt_id);
        agent.prompt.set_text("");
        agent.note_draft_consumed();

        let sid_str = session_id.0.to_string();
        push_server_queue_echo(app, agent_id, &sid_str, &prompt_id, &command, "bash");
        crate::unified_log::info(
            "prompt.send_server_authoritative",
            Some(&sid_str),
            Some(serde_json::json!({ "kind": "bash", "len": command.len() })),
        );
        return vec![Effect::SendBashCommand {
            agent_id,
            session_id,
            command,
            prompt_id,
        }];
    }

    agent.session.enqueue_bash_command(command.clone());
    agent.prompt.set_text("");
    agent.note_draft_consumed();

    let drain = maybe_drain_queue(agent);
    note_peek_page_flip(app, id, drain.page_flip_entry);
    drain.effects
}

/// Whether a load-result handler must stand down because a reconnect reload window is open on the agent.
/// A load result resolving mid-window (a stale fresh-view load, or `/resume` racing a reconnect) must not close it.
/// Flipping `loading_replay` would make the replay gate drop the rest of the reconnect replay, and a failure block would be pushed into staging state.
pub(super) fn defer_to_open_reload_window(
    agent: &AgentView,
    agent_id: AgentId,
    result: &str,
) -> bool {
    if agent.session_reload.is_none() {
        return false;
    }
    tracing::warn!(
        agent = ?agent_id,
        result,
        "load result during an open reload window — deferring to the window finalize"
    );
    true
}

/// The initiation-side counterpart of [`defer_to_open_reload_window`].
/// A load INITIATION that takes over the agent (fork/worktree-fork/remote-restore binding a session) finalizes any open reload window as failed first.
/// Unreachable through today's flows: these arms target freshly created `session_id: None` agents, which can never host a window.
pub(super) fn supersede_open_reload_window(
    agent: &mut AgentView,
    agent_id: AgentId,
    initiation: &str,
) {
    if agent.session_reload.is_none() {
        return;
    }
    tracing::warn!(
        agent = ?agent_id,
        initiation,
        "load initiation supersedes an open reload window (finalizing as failed)"
    );
    agent.abort_session_reload();
}

// TaskResult handlers.

pub(super) fn handle_prompt_response(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<acp::PromptResponse, String>,
    http_status: Option<u16>,
    prompt_id: Option<String>,
) -> Vec<Effect> {
    // A server-authoritative queued prompt may have drained into the running slot while this turn was still finishing
    // The leader's `running_prompt_id` broadcast can arrive before this `PromptResponse`
    // Take any stashed adoption now; it is applied after `finish_turn` clears `current_prompt_id` below
    let pending_adoption = app.pending_running_adoptions.remove(&agent_id);
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        // Discard PromptResponses that don't belong to the currently active prompt
        // They belong to a turn the user rewound, or to a queued prompt that never became the running turn
        // Without the `Err` fallback, a queued prompt's RPC error has no id to gate on and is misattributed to the running turn
        let response_pid = match &result {
            Ok(pr) => pr
                .meta
                .as_ref()
                .and_then(|m| m.get("promptId"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            Err(_) => prompt_id.clone(),
        };
        // The turn-end RPC for this prompt arrived: clear the lost-response reconcile that `handle_prompt_complete` set for it
        // The broadcast is emitted before the RPC response, so in the healthy path the marker lives only a few ms
        if let Some(pending) = agent.pending_turn_end_reconcile.as_ref()
            && response_pid.as_deref() == Some(pending.prompt_id.as_str())
        {
            agent.pending_turn_end_reconcile = None;
        }
        if let Some(response_pid) = response_pid.as_deref()
            && agent.session.current_prompt_id.as_deref() != Some(response_pid)
        {
            if (agent.session.current_prompt_id.is_none()
                || agent
                    .session
                    .current_prompt_id
                    .as_deref()
                    .is_some_and(crate::app::acp_handler::is_server_initiated_prompt))
                && crate::app::acp_handler::is_server_initiated_prompt(response_pid)
            {
                // Server-initiated turn (auto-wake): adopt
                agent.session.current_prompt_id = Some(response_pid.to_string());
            } else {
                // Not the running turn: this response (Ok rewound/stale, or Err from a queued/removed prompt) must not touch the active turn
                // Restore the adoption we popped above so a genuinely-draining next prompt can still be adopted by the real running turn's PromptResponse
                // Unless it is the stashed turn's own response: that spent the turn's only exit, so consume (discard), never restore
                if let Some(p) = pending_adoption {
                    if p.prompt_id == response_pid {
                        agent.discard_pending_adoption_updates(&p.prompt_id);
                    } else {
                        app.pending_running_adoptions.insert(agent_id, p);
                    }
                }
                // This prompt's RPC resolved without becoming the running turn (removed, cancelled, rewound)
                // Retire its optimistic echo so a later `x.ai/queue/changed` broadcast can't re-pin a stale placeholder and reorder the queue
                if let Some(sid) = agent.session.session_id.as_ref().map(|s| s.0.to_string()) {
                    retire_optimistic_echo(
                        &mut app.optimistic_prompt_echoes,
                        &mut app.shared_prompt_queues,
                        &sid,
                        response_pid,
                    );
                    agent.shared_queue.retain(|e| e.id != response_pid);
                    agent.note_queue_echo_retired(response_pid);
                }
                // Resolved-without-running never adopts; explicit for the session-less arm (no note_queue_echo_retired above)
                // Exception: an active-goal Send Now painted block still awaiting its interjection claim stays put
                // Retiring it here (before that claim wins the race) would drop and re-push the message at the scrollback end
                if !agent.is_send_now_awaiting_interjection_claim(response_pid) {
                    agent.retire_send_now_painted_block(response_pid);
                }
                return vec![];
            }
        }
        let was_cancelling = agent.session.state.is_cancelling()
            || matches!(
                &result,
                Ok(pr) if pr.stop_reason == acp::StopReason::Cancelled
            );
        // Send-now cancel: suppress the "Turn cancelled by user" marker (the new prompt follows right under the partial)
        // Wire `cancelTrigger` wins, else the client-side expectation; consumed at every turn end (no stale flag)
        let expected_send_now = agent.expect_send_now_cancel.take();
        let wire_cancel_trigger = result.as_ref().ok().and_then(|pr| {
            pr.meta
                .as_ref()?
                .get(crate::app::turn_completion::CANCEL_TRIGGER_KEY)?
                .as_str()
                .map(str::to_string)
        });
        let send_now_cancel = was_cancelling
            && match wire_cancel_trigger.as_deref() {
                Some(trigger) => trigger == "send_now",
                None => expected_send_now.is_some(),
            };
        // `RemovedFromQueue` is also `Cancelled` on the wire; only the stamped
        // kind is silent. A newer wake is not evidence this response was a
        // queue removal (it can land before a delayed PromptResponse).
        let removed_from_queue = was_cancelling
            && result.as_ref().ok().is_some_and(|pr| {
                pr.meta
                    .as_ref()
                    .and_then(|m| m.get(crate::app::turn_completion::COMPLETION_KIND_KEY))
                    .and_then(|v| v.as_str())
                    == Some(crate::app::turn_completion::REMOVED_FROM_QUEUE_KIND)
            });
        let suppress_cancel_marker = send_now_cancel || removed_from_queue;
        // A hook-denied end arrives with the cancelled stop reason but is a policy block, not a user cancel; `cancelled_turn_event` picks the marker
        let wire_cancellation_category = result.as_ref().ok().and_then(|pr| {
            pr.meta
                .as_ref()?
                .get(crate::app::turn_completion::CANCELLATION_CATEGORY_KEY)?
                .as_str()
                .map(str::to_string)
        });
        let wire_cancellation_context = result.as_ref().ok().and_then(|pr| {
            pr.meta
                .as_ref()?
                .get(crate::app::turn_completion::CANCELLATION_CONTEXT_KEY)
                .cloned()
        });
        crate::app::turn_completion::note_hook_blocked_turn(
            agent,
            // A reply without the server-stamped id still names this client's own request: never leave the self/foreign check without an id
            response_pid.as_deref().or(prompt_id.as_deref()),
            wire_cancellation_category.as_deref(),
            wire_cancellation_context.as_ref(),
        );
        let rate_limited = agent.session.rate_limited;
        // Fallback mirroring the credit-limit race guard below
        // If the retry notification lost the race with (or never reached) this PromptResponse, detect the free-usage code from the prompt error itself
        // The flattened 429 body embeds it
        let free_usage_blocked = agent.session.free_usage_blocked
            || result
                .as_ref()
                .err()
                .is_some_and(|e| distill_shell::sampling::error::is_free_usage_exhausted_error(e));
        let model_incompatible = agent.session.model_incompatible;
        // Context overflow: the RetryState handler already pushed the actionable block, so the generic TurnFailed and error toast are redundant
        // Derived from the scrollback (mirrors reauth), not a session flag
        let context_overflow = scrollback_has_recent_context_too_large(&agent.scrollback);
        let disk_full_from_error = result
            .as_ref()
            .err()
            .is_some_and(|e| crate::app::effects::is_disk_full_error(e));
        if disk_full_from_error && !scrollback_has_recent_disk_full(&agent.scrollback) {
            agent
                .scrollback
                .push_block(RenderBlock::session_event(SessionEvent::DiskFull));
        }
        let disk_full = disk_full_from_error || scrollback_has_recent_disk_full(&agent.scrollback);
        // Fallback for when the retry notification didn't set the flag
        // Detect credit-limit denials (legacy 403 or pool 402) from the PromptResponse error and HTTP status
        // The error text is already banner-formatted ("Request failed (402): …"), so recover the status from it when the field is absent
        let credit_limit_blocked = agent.session.credit_limit_blocked
            || result.as_ref().err().is_some_and(|e| {
                let status =
                    http_status.or_else(|| crate::app::error_display::parse_http_status(e));
                is_credit_limit_error(status, e)
            });
        // A 401/auth failure already showed an actionable `ReAuthRequired` prompt via the RetryState handler (which runs before this PromptResponse)
        // Suppress the redundant "Turn failed" block and error toast so only the prompt shows
        // The needle matches both the raw "Unauthorized (401)" dump and the banner-formatted "Request failed (401): …" text
        let reauth_prompted = scrollback_has_recent_reauth_prompt(&agent.scrollback)
            || (http_status == Some(401)
                && result.as_ref().err().is_some_and(|e| {
                    e.contains(distill_shell::extensions::notification::HTTP_401_NEEDLE)
                }));
        let request_failed_shown = scrollback_has_recent_request_failed(&agent.scrollback);
        // A dedicated prompt/modal/banner replaces the generic TurnFailed marker and error toast
        // The cases: rate limit, free-usage paywall, model incompatibility, credit 402/403, 401 re-auth, context overflow, and disk-full
        // A formatted RequestFailed banner from RetryState also counts
        let dedicated_ux_shown = rate_limited
            || free_usage_blocked
            || model_incompatible
            || credit_limit_blocked
            || reauth_prompted
            || context_overflow
            || disk_full
            || request_failed_shown;
        let elapsed = agent.turn_elapsed();

        {
            let sid = agent.session.session_id.as_ref().map(|s| s.0.as_ref());
            let elapsed_ms = elapsed.map(|d| d.as_millis() as u64).unwrap_or(0);
            let ok = result.is_ok();
            crate::unified_log::info(
                "turn.complete",
                sid,
                Some(serde_json::json!({
                    "elapsed_ms": elapsed_ms,
                    "ok": ok,
                    "was_cancelling": was_cancelling,
                    "send_now_cancel": send_now_cancel,
                    "removed_from_queue": removed_from_queue,
                })),
            );
        }

        // Stash the complete in-flight prompt before finish_turn clears it.
        // Used by CreditLimitRecheckComplete to retry after a tier upgrade.
        if credit_limit_blocked && let Some(prompt) = agent.session.in_flight_prompt.clone() {
            agent.credit_limit_stashed_prompt = Some(prompt);
        }
        // Stash for AuthComplete after 401
        // Prefer in_flight; fall back to compact_held (cleared for cancel-rewind during auto-compact)
        // Skip if both None
        if reauth_prompted {
            let held = agent
                .session
                .in_flight_prompt
                .clone()
                .or_else(|| agent.session.compact_held_prompt.clone());
            if let Some(prompt) = held {
                agent.reauth_stashed_prompt = Some(prompt);
            }
        }

        // qtrace: turn end on this client
        // This clears current_prompt_id and (briefly) returns the client to Idle, the start of the leader-mode turn-end window
        // In that window a freshly-sent prompt can be wrongly local-drained before the next running-prompt broadcast is adopted
        tracing::debug!(
            target: "qtrace",
            pid = std::process::id(),
            event = "turn_end",
            prompt_id = prompt_id.as_deref().unwrap_or(""),
            was_cancelling,
            shared_queue_len = agent.shared_queue.len(),
            pending_len = agent.session.pending_prompts.len(),
            has_pending_adoption = pending_adoption.is_some(),
            session = agent.session.session_id.as_ref().map(|s| s.0.as_ref()).unwrap_or(""),
            "turn ended; client returning to idle",
        );

        agent.session.finish_turn(&mut agent.scrollback);

        // Insert the session event message (skip TurnCompleted for bash-mode, which has no agent turn)
        let event = match (&result, was_cancelling) {
            (Ok(_), false) if agent.bash_turn => None,
            (Err(_), _) if dedicated_ux_shown => None,
            // `err` is already banner-formatted by `format_acp_error` at the producer, the single formatting owner
            // Don't re-format here
            (Err(err), _) => Some(SessionEvent::TurnFailed {
                error: err.clone(),
                elapsed,
            }),
            (Ok(_), _) => {
                let stop = if was_cancelling {
                    crate::app::turn_completion::TurnStopReason::Cancelled
                } else {
                    crate::app::turn_completion::TurnStopReason::EndTurn
                };
                crate::app::turn_completion::terminal_marker(
                    crate::app::turn_completion::TerminalMarkerInput {
                        stop,
                        elapsed_ms: crate::app::turn_completion::duration_to_elapsed_ms(elapsed),
                        agent_result: None,
                        send_now_cancel: suppress_cancel_marker,
                        cancel_trigger: wire_cancel_trigger.as_deref(),
                        cancellation_category: wire_cancellation_category.as_deref(),
                        // Ok-path marker: the Error arm is unreachable here.
                        error_kind: None,
                        error_banner_present: false,
                    },
                )
            }
        };
        crate::app::turn_completion::push_turn_terminal_marker(agent, event);

        let notification = match (&result, was_cancelling) {
            (Ok(_), false) if !agent.bash_turn => {
                let body = match elapsed {
                    Some(d) => {
                        format!("Turn complete in {}.", crate::util::format_duration(d))
                    }
                    None => String::from("Turn complete."),
                };
                Some((NotificationEventKind::TurnComplete, body))
            }
            (Err(err), _) if !dedicated_ux_shown => {
                Some((NotificationEventKind::AgentError, format!("Error: {err}")))
            }
            _ => None,
        };

        let was_bash_turn = agent.bash_turn;
        finish_turn_view(agent, TurnEnd::Completed);

        // In-turn reviews die with their ext method. A post-turn review is the waiting UI and stays.
        // Cancelled or failed turns do not open approve/build for an in-progress or rejected plan.
        let dismissed_in_turn_review = agent.dismiss_in_turn_plan_review();
        // Snapshot before take: Default already dropped the review, so a
        // later user Plan entry must not be yanked without set_mode.
        let leave_after_lost_default = agent.is_post_turn_build_starting();
        let execute_plan_settled = agent.take_execute_plan_prompt(prompt_id.as_deref());
        let completed_end_turn = matches!(
            &result,
            Ok(pr) if !was_cancelling && pr.stop_reason == acp::StopReason::EndTurn
        );
        if execute_plan_settled {
            // Failed or cancelled start keeps the original review. Do
            // not treat !plan_mode_active as proof: a user Default can
            // clear that before ExecutePlan refuses.
            if completed_end_turn {
                agent.commit_post_turn_plan_approved();
                agent.clear_kept_plan();
                if leave_after_lost_default {
                    agent.leave_plan_after_approved_build();
                }
            } else {
                agent.pending_post_turn_commit = None;
            }
        } else if !dismissed_in_turn_review {
            // EndTurn, cancel, and fail all open review when a keep exists.
            // has_kept_plan / plan-mode guards live in open_post_turn_plan_review.
            agent.open_post_turn_plan_review();
        }

        // TurnComplete suppressed when the queue is non-empty (the badge fires only after the final queued turn); AgentError always fires
        if let Some((kind, body)) = notification {
            // A stashed server-authoritative adoption means the next turn is about to start
            // So treat the queue as non-empty (suppress TurnComplete and the idle escapes), mirroring the local non-empty-queue behavior
            let queue_empty =
                agent.session.pending_prompts.is_empty() && pending_adoption.is_none();
            let session_name = agent
                .display_name
                .as_deref()
                .or(agent.generated_session_title.as_deref());

            // Skip idle escapes when the queue is non-empty: the next turn starts immediately and would overwrite them (title flicker)
            if queue_empty {
                let cwd_str = app.cwd.to_string_lossy();
                let model = agent.session.models.current_model_name();
                let idle_title = crate::notifications::TitleState {
                    session_name,
                    model: model.as_deref(),
                    activity: None,
                    has_pending_permissions: false,
                    cwd: Some(&cwd_str),
                    turn_elapsed: None,
                    is_busy: false,
                    focused: true,
                };
                app.pending_notification_escapes =
                    app.notification_service.build_idle_escapes(&idle_title);
            }

            if kind != NotificationEventKind::TurnComplete || queue_empty {
                // Defer the notification so the terminal has time to apply the idle title
                // Ghostty debounces setTitle() by 75 ms (SurfaceView_AppKit.swift:576)
                // So we need more than 75 ms before the notification reads self.title for the subtitle; 3 ticks × 33 ms ≈ 99 ms
                let session_id = agent.session.session_id.as_ref().map(|s| s.0.to_string());

                // Use the session name as the notification title so terminals that show it (Ghostty/OSC 777) display which session completed
                // For body-only protocols (Warp, iTerm2/OSC 9), emit_notification folds the title into the body automatically
                let notif_title = session_name
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Grok".into());

                app.deferred_notification = Some((
                    NotificationEvent {
                        kind,
                        title: notif_title,
                        body,
                        session_id,
                    },
                    3,
                ));
            }
        }

        if let Err(ref err) = result {
            tracing::error!(agent = ?agent_id, error = %err, "Prompt failed");
        }

        // Predicted-next-prompt (tab autocomplete): wipe any stale suggestion at every turn boundary
        // This must run before the reconnect and credit-limit early returns below, which skip the fetch gate entirely
        // A prior ghost would otherwise survive those paths
        agent.prompt.prompt_suggestion.clear();

        // Cancelled turns resume queue processing one item at a time through the same drain path as normal completions
        // `maybe_drain_queue` keeps the idle-only and editing-front guards so we do not send from under the user
        if app.reconnect_pending {
            if let Some(p) = pending_adoption {
                agent.discard_pending_adoption_updates(&p.prompt_id);
            }
            return vec![];
        }

        // Credit-limit (403 legacy / 402 pool): strip stale error blocks, then do a one-shot subscription re-check
        // If the tier changed (user upgraded mid-session), the stashed prompt is retried automatically; otherwise the upsell is shown
        if credit_limit_blocked {
            // Strip stale error blocks that were pushed before the credit-limit was detected
            let to_remove: Vec<usize> = super::auth::trailing_session_events(&agent.scrollback)
                .filter(|(_, ev)| {
                    matches!(
                        ev,
                        SessionEvent::RequestFailed { .. }
                            | SessionEvent::RetryFailed { .. }
                            | SessionEvent::TurnFailed { .. }
                    )
                })
                .map(|(idx, _)| idx)
                .collect();
            for idx in to_remove {
                agent.scrollback.remove_from(idx);
            }

            // Defer the upsell until the subscription re-check completes
            // Queue drain and billing fetch happen in the CreditLimitRecheckComplete handler
            if let Some(p) = pending_adoption {
                agent.discard_pending_adoption_updates(&p.prompt_id);
            }
            return vec![Effect::CreditLimitRecheck { agent_id }];
        }

        // Free-usage paywall (a 429 with subscription:free-usage-exhausted)
        // Driver-only by construction: viewers never receive a PromptResponse
        // No queue drain: queued prompts would fail on the same exhausted quota
        if free_usage_blocked {
            let auth_method = app.login_method_id.as_ref().map(|id| id.0.to_string());
            super::billing::open_free_usage_upsell(agent, auth_method);
            if let Some(p) = pending_adoption {
                agent.discard_pending_adoption_updates(&p.prompt_id);
            }
            return vec![];
        }

        // FIFO order: a server-authoritative prompt may have drained into the running slot during this turn's teardown
        // Adopt it now (finish_turn cleared current_prompt_id) and run the turn-start shim
        // This sets `TurnRunning`, so the `maybe_drain_queue` below no-ops rather than draining a local prompt; the leader owns the drain order
        let adopted_page_flip = if let Some(p) = pending_adoption
            && agent.session.current_prompt_id.is_none()
        {
            if response_pid.as_deref() != Some(p.prompt_id.as_str())
                && agent.should_adopt_running_prompt(&p.prompt_id)
            {
                apply_turn_start_shim(agent, p.prompt_id, p.text, &p.kind, p.combined_texts)
            } else {
                agent.discard_pending_adoption_updates(&p.prompt_id);
                None
            }
        } else {
            None
        };

        let drain = maybe_drain_queue(agent);
        let page_flip_entry = adopted_page_flip.or(drain.page_flip_entry);
        let mut effects = drain.effects;

        // Predicted-next-prompt (tab autocomplete): fetch a fresh suggestion (the stale one was wiped above)
        // It fetches only after a clean, non-bash agent turn that leaves the session idle with an empty prompt and no queued work, local or server-side
        // A draft in progress or a draining queue means the user is already mid-thought
        if crate::views::prompt_suggestion::resolve_enabled()
            && result.is_ok()
            && !was_cancelling
            && !was_bash_turn
            && agent.prompt.text().is_empty()
            && agent.session.pending_prompts.is_empty()
            && agent.shared_queue.is_empty()
            && agent.session.state.is_idle()
            && let Some(session_id) = agent.session.session_id.as_ref().map(|s| s.0.to_string())
        {
            let generation = agent.prompt.prompt_suggestion.begin_fetch();
            let model = crate::views::prompt_suggestion::resolve_model();
            effects.push(Effect::FetchPromptSuggestion {
                agent_id,
                generation,
                model,
                session_id: Some(session_id),
            });
        }

        effects.push(Effect::FetchBilling {
            agent_id,
            silent: true,
            nonce: Default::default(),
        });
        note_peek_page_flip(app, agent_id, page_flip_entry);
        return effects;
    }
    vec![]
}

/// `/flush` or `/dream` finished: close the command state and post its outcome line.
pub(super) fn handle_memory_command_complete(
    app: &mut AppView,
    agent_id: AgentId,
    kind: MemoryCommandKind,
    result: Result<(String, bool), String>,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    let command = match kind {
        MemoryCommandKind::Flush => AgentCommand::MemoryFlush,
        MemoryCommandKind::Dream => AgentCommand::MemoryDream,
    };
    if agent.session.state.command_in_flight() != Some(&command) {
        tracing::debug!(
            ?command,
            "Ignoring memory command result (not in its command state)"
        );
        return vec![];
    }
    let elapsed = agent.turn_elapsed().unwrap_or_default();
    agent.session.finish_command();

    let (summary, succeeded) = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::error!(agent = ?agent_id, ?command, %error, "Memory command failed");
            (error, false)
        }
    };
    agent.scrollback.push_block(RenderBlock::session_event(
        SessionEvent::MemoryCommandCompleted {
            summary,
            succeeded,
            elapsed,
        },
    ));

    agent.mark_turn_finished(TurnEnd::Completed);
    agent.activity_started_at = None;
    agent.last_activity = None;

    if app.reconnect_pending {
        return vec![];
    }
    let drain = maybe_drain_queue(agent);
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    drain.effects
}

pub(super) fn handle_compact_complete(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<(), crate::app::effects::CompactError>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        // Defensive: only process if we're still in a compact command state.
        let was_cancelling = matches!(
            agent.session.state,
            AgentState::CommandCancelling {
                command: AgentCommand::Compact,
            }
        );
        if !matches!(
            agent.session.state,
            AgentState::CommandRunning {
                command: AgentCommand::Compact,
                ..
            } | AgentState::CommandCancelling {
                command: AgentCommand::Compact,
            }
        ) {
            tracing::debug!("Ignoring CompactComplete (not in compact command state)");
            return vec![];
        }

        let elapsed = agent.turn_elapsed();
        agent.session.finish_command();

        match &result {
            Ok(()) => {
                agent.scrollback.push_block(RenderBlock::session_event(
                    SessionEvent::CompactCompleted {
                        elapsed: elapsed.unwrap_or_default(),
                    },
                ));
            }
            // Typed kind with old-shell text fallback, per `compact_error`.
            Err(err) if was_cancelling || err.cancelled => {
                agent.scrollback.push_block(RenderBlock::session_event(
                    SessionEvent::CompactionCancelled,
                ));
            }
            Err(err) => {
                tracing::error!(agent = ?agent_id, error = %err.message, "Compaction failed");
                // The message is already sanitized and capped by the effect layer
                agent.scrollback.push_block(RenderBlock::session_event(
                    SessionEvent::CompactionFailed {
                        error: err.message.clone(),
                    },
                ));
            }
        }

        agent.mark_turn_finished(TurnEnd::Completed);
        agent.activity_started_at = None;
        agent.last_activity = None;

        if app.reconnect_pending {
            return vec![];
        }
        let drain = maybe_drain_queue(agent);
        note_peek_page_flip(app, agent_id, drain.page_flip_entry);
        return drain.effects;
    }
    vec![]
}

pub(super) fn handle_suggestion_debounce_expired(
    app: &mut AppView,
    agent_id: AgentId,
    generation: u64,
) -> Vec<Effect> {
    // Route by the agent that set the timer (the timer carries it), not the active view
    // A view switch inside the debounce window must neither fire a spurious fetch on another agent nor drop this one's
    let Some(agent) = app.agents.get(&agent_id) else {
        return vec![];
    };
    // Bash-mode feature: a debounce that outlives the mode fetches nothing.
    if agent.prompt_input_mode != crate::app::agent_view::PromptInputMode::Bash {
        return vec![];
    }
    if !agent.prompt.suggestions.on_debounce_expired(generation) {
        return vec![];
    }
    let text = agent.prompt.text().to_owned();
    let cursor = agent.prompt.cursor();
    let cwd = agent.session.cwd.to_string_lossy().into_owned();
    let include_ai = agent.prompt.suggestions.ai_enabled;
    let ai_model = agent.prompt.suggestions.ai_model.clone();
    let session_id = agent.session.session_id.as_ref().map(|s| s.0.to_string());
    vec![Effect::FetchShellSuggestions {
        agent_id,
        text,
        cursor,
        cwd,
        generation,
        limit: crate::views::suggestion_controller::SHELL_SUGGEST_WIRE_LIMIT,
        include_ai,
        ai_model,
        session_id,
        // The as-you-type (ghost) suggestions keep the history/AI providers
        token_only: false,
    }]
}
