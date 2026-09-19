// Modified for Distill by Samuel Fajreldines, 2026.
//! Session status, sharing, privacy, usage, and info dispatchers.

use agent_client_protocol as acp;

use super::ctx::get_active_agent;
use super::queue::push_and_page_flip;
use super::settings::ui::refresh_open_settings_modals;
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView, PendingCodingDataWrite};
use crate::notifications::{NotificationEvent, NotificationEventKind};
use crate::scrollback::block::RenderBlock;

/// Temporary kill switch: client share links are disabled.
pub(super) fn dispatch_share_session(app: &mut AppView) -> Vec<Effect> {
    app.show_toast("Session sharing is temporarily disabled");
    vec![]
}

/// Monotonic generation for usage-modal fetches, shared by every surface that opens the modal.
/// A reply from a previous open (modal closed and reopened) then can't overwrite newer results.
/// `0` is reserved for background refreshes (minimal-mode paths, startup/login `FetchAppBilling`), which never settle a modal.
static USAGE_FETCH_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_usage_fetch_nonce() -> u64 {
    USAGE_FETCH_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// The agent's open usage modal state, if any.
pub(super) fn usage_modal_state_mut(
    agent: &mut AgentView,
) -> Option<&mut crate::views::usage_modal::UsageInfoModalState> {
    match agent.active_modal.as_mut() {
        Some(crate::views::modal::ActiveModal::UsageInfo { state }) => Some(state),
        _ => None,
    }
}

/// Open (or re-tab) the usage/session-info modal and fire the fetch effects that populate it.
/// Full-TUI only; minimal mode keeps scrollback blocks.
pub(super) fn open_usage_info_modal(
    app: &mut AppView,
    tab: crate::views::usage_modal::UsageInfoTab,
) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;
    use crate::views::usage_modal::{UsageInfoContext, UsageInfoModalState};

    if matches!(app.active_view, ActiveView::AgentDashboard) {
        return open_dashboard_usage_modal(app, tab);
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let usage_visible = app.usage_visible;
    let redirect_url = app.usage_billing_redirect_url.clone();
    let tier = app.subscription_tier.clone();
    let show_resolved_model = app.show_resolved_model;
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let session_id = agent.session.session_id.clone();

    if let Some(state) = usage_modal_state_mut(agent) {
        state.set_tab(tab);
        return vec![];
    }

    let billing_reachable = usage_visible && !agent.chat_kind && redirect_url.is_none();
    let nonce = next_usage_fetch_nonce();
    let mut state = UsageInfoModalState::new(
        tab,
        UsageInfoContext {
            session_id: session_id.as_ref().map(|s| s.0.to_string()),
            usage_visible,
            chat_kind: agent.chat_kind,
            billing_redirect_url: redirect_url,
            subscription_tier: tier,
        },
    );
    state.fetch_nonce = nonce;

    let mut effects = Vec::new();
    if let Some(session_id) = session_id {
        effects.push(Effect::ShowContextInfo {
            agent_id: id,
            session_id: session_id.clone(),
            nonce,
        });
        effects.push(Effect::ShowSessionInfo {
            agent_id: id,
            session_id: session_id.clone(),
            show_resolved_model,
            nonce,
        });
        effects.push(Effect::FetchSessionUsage {
            agent_id: id,
            session_id,
            nonce,
        });
    }
    // Silently refresh the cached billing mirrors the modal renders from
    if billing_reachable {
        state.billing_loading = true;
        effects.push(Effect::FetchBilling {
            agent_id: id,
            silent: true,
            nonce,
        });
    }
    agent.active_modal = Some(ActiveModal::UsageInfo {
        state: Box::new(state),
    });
    effects
}

/// Session-less variant: no session tabs to fetch, so only the account allowance is refreshed (agent-less `FetchAppBilling`).
/// `chat_kind` follows the process-wide `--chat` flag, which is what every session created from this dashboard would carry.
fn open_dashboard_usage_modal(
    app: &mut AppView,
    tab: crate::views::usage_modal::UsageInfoTab,
) -> Vec<Effect> {
    use crate::views::usage_modal::{UsageInfoContext, UsageInfoModalState};

    let chat_kind = app.chat_mode;
    let billing_reachable =
        app.usage_visible && !chat_kind && app.usage_billing_redirect_url.is_none();
    let ctx = UsageInfoContext {
        session_id: None,
        usage_visible: app.usage_visible,
        chat_kind,
        billing_redirect_url: app.usage_billing_redirect_url.clone(),
        subscription_tier: app.subscription_tier.clone(),
    };
    let Some(dashboard) = app.dashboard.as_mut() else {
        return vec![];
    };
    if let Some(state) = dashboard.usage_modal.as_mut() {
        state.set_tab(tab);
        return vec![];
    }
    let mut state = UsageInfoModalState::new(tab, ctx);
    let mut effects = Vec::new();
    if billing_reachable {
        let nonce = next_usage_fetch_nonce();
        state.fetch_nonce = nonce;
        state.billing_loading = true;
        effects.push(Effect::FetchAppBilling { nonce });
    }
    dashboard.usage_modal = Some(Box::new(state));
    effects
}

/// `/session-info`: open the usage modal on its "Session info" tab, or fetch-and-show in scrollback in minimal mode.
pub(super) fn dispatch_show_session_info(app: &mut AppView) -> Vec<Effect> {
    if !app.screen_mode.is_minimal() {
        return open_usage_info_modal(app, crate::views::usage_modal::UsageInfoTab::SessionInfo);
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        // No active session; the slash command should have caught this, but guard here just in case
        return vec![];
    };

    vec![Effect::ShowSessionInfo {
        agent_id: id,
        session_id,
        show_resolved_model: app.show_resolved_model,
        nonce: Default::default(),
    }]
}

/// State-only mutation for `coding_data_sharing`; the shell owns the setting.
pub(super) fn set_coding_data_sharing_inner(app: &mut AppView, opted_in: bool) {
    app.coding_data_retention_opt_out = !opted_in;
}

/// Agent the coding-data ACP write is attributed to.
/// Privacy is app-level, so the id only routes the result back.
/// `AgentId(0)` is the synthetic stand-in for the welcome screen, where the banner is reachable before a session exists.
fn coding_data_sharing_agent_id(app: &AppView) -> AgentId {
    match app.active_view {
        ActiveView::Agent(id) => id,
        _ => app.agents.keys().next().copied().unwrap_or(AgentId(0)),
    }
}

/// Claim the next write generation.
/// Every `SetCodingDataSharing` must take one so its reply can be matched against the newest write.
fn next_coding_data_write_seq(app: &mut AppView) -> u64 {
    app.coding_data_write_seq += 1;
    app.coding_data_write_seq
}

/// Is this reply from the newest write?
/// Writes to this endpoint run concurrently and can land out of order, so an older reply must not set the mirror.
fn is_current_coding_data_write(app: &AppView, seq: u64, agent_id: AgentId) -> bool {
    if seq == app.coding_data_write_seq {
        return true;
    }
    tracing::debug!(
        target: "settings",
        key = "coding_data_sharing",
        ?agent_id,
        seq,
        current = app.coding_data_write_seq,
        "dropping superseded coding-data reply",
    );
    false
}

fn log_coding_data_consent_selected(
    source: distill_telemetry::events::CodingDataConsentSource,
    opted_in: bool,
    previous_opted_in: bool,
) {
    use distill_telemetry::events::{CodingDataConsentChoice, CodingDataConsentSelected};
    distill_telemetry::session_ctx::log_event(CodingDataConsentSelected {
        source,
        choice: CodingDataConsentChoice::from_opted_in(opted_in),
        previous_choice: CodingDataConsentChoice::from_opted_in(previous_opted_in),
        changed: opted_in != previous_opted_in,
    });
}

/// Set coding-data-sharing preference.
/// The shell owns this setting and stores it in auth metadata (persists via ACP ext-request, NOT `~/.grok/config.toml`).
pub(super) fn set_coding_data_sharing(
    app: &mut AppView,
    opted_in: bool,
    source: distill_telemetry::events::CodingDataConsentSource,
) -> Vec<Effect> {
    if app.is_zdr {
        app.show_toast("\u{2717} Cannot change: Zero Data Retention enabled");
        return vec![];
    }
    if app.team_name.is_some() {
        let is_admin = app
            .team_role
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case("admin"));
        if !is_admin {
            app.show_toast("\u{2717} Data sharing is controlled by your team admin");
            return vec![];
        }
    }
    let agent_id = coding_data_sharing_agent_id(app);
    let prev = !app.coding_data_retention_opt_out;
    log_coding_data_consent_selected(source, opted_in, prev);

    // Coalesce on the pending write's own choice, not the mirror, which auth-meta refreshes rewrite mid-flight; a duplicate has nothing new to send
    if app.coding_data_pending_opted_in() == Some(opted_in) {
        return vec![];
    }
    // Only an idle opt-in may skip the write: a local "out" can be the unconfirmed fail-safe default, so an opt-out always writes
    if opted_in && prev && app.coding_data_pending_write.is_none() {
        return ack_privacy_banner(app);
    }

    // A replaced write hands its rollback on; `prev` here would be the replaced write's unconfirmed optimistic value
    let rollback_to_opted_in = app
        .coding_data_pending_write
        .map_or(prev, |w| w.rollback_to_opted_in);
    app.coding_data_pending_write = Some(PendingCodingDataWrite {
        opted_in,
        rollback_to_opted_in,
    });

    // Optimistic mutation
    // Success is silent; only the refusals above and the failure handler toast
    set_coding_data_sharing_inner(app, opted_in);
    refresh_open_settings_modals(app);

    tracing::info!(
        target: "settings",
        key = "coding_data_sharing",
        opted_in,
        "setting changed",
    );

    let seq = next_coding_data_write_seq(app);
    vec![Effect::SetCodingDataSharing {
        agent_id,
        opted_in,
        seq,
    }]
}

/// Scrub an untrusted error string for toast display.
/// Substitutes a generic placeholder when the input exceeds 120 chars or contains control / bidi-override characters.
/// That prevents escape-sequence injection and visual spoofing.
pub(super) fn scrub_error_for_toast(error: &str) -> String {
    const MAX_TOAST_ERROR_LEN: usize = 120;
    if error.len() > MAX_TOAST_ERROR_LEN
        || error
            .chars()
            .any(crate::render::line_utils::is_unsafe_display_char)
    {
        "server error (see logs for details)".to_string()
    } else {
        error.to_string()
    }
}

/// `/context` and the context-bar click: open the usage modal on its "Context usage" tab, or fetch-and-show in scrollback in minimal mode.
pub(super) fn dispatch_show_context_info(app: &mut AppView) -> Vec<Effect> {
    if !app.screen_mode.is_minimal() {
        return open_usage_info_modal(app, crate::views::usage_modal::UsageInfoTab::ContextUsage);
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };

    vec![Effect::ShowContextInfo {
        agent_id: id,
        session_id,
        nonce: Default::default(),
    }]
}

/// `/usage`: open the usage modal on its "Usage limit" tab.
/// Minimal mode keeps the scrollback flow: session token/cost, then consumer credits.
pub(super) fn dispatch_show_usage(app: &mut AppView) -> Vec<Effect> {
    if !app.screen_mode.is_minimal() {
        return open_usage_info_modal(app, crate::views::usage_modal::UsageInfoTab::UsageLimit);
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let session_id = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        agent.session.session_id.clone()
    };
    match session_id {
        Some(session_id) => vec![Effect::FetchSessionUsage {
            agent_id: id,
            session_id,
            nonce: Default::default(),
        }],
        None => {
            if let Some(agent) = app.agents.get_mut(&id) {
                push_and_page_flip(
                    &mut agent.scrollback,
                    RenderBlock::system(
                        "Session usage is unavailable until the session starts.".to_string(),
                    ),
                );
            }
            append_consumer_billing_surface(app, id)
        }
    }
}

/// Route a session-usage result (success or failure text) into the open usage modal, or into scrollback in minimal mode.
/// Stale results are dropped.
pub(super) fn handle_session_usage_result(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: &acp::SessionId,
    text: String,
    nonce: u64,
) -> Vec<Effect> {
    if !app.screen_mode.is_minimal() {
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            if agent.session.session_id.as_ref() != Some(session_id) {
                return vec![];
            }
            if let Some(state) = usage_modal_state_mut(agent)
                && state.fetch_nonce == nonce
            {
                state.session_usage_text = Some(text);
            }
        }
        return vec![];
    }
    commit_session_usage_block(app, agent_id, session_id, text)
}

/// Commit a session-usage block if still on `session_id`, then consumer credits.
pub(super) fn commit_session_usage_block(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: &acp::SessionId,
    text: String,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if agent.session.session_id.as_ref() != Some(session_id) {
        return vec![];
    }
    push_and_page_flip(&mut agent.scrollback, RenderBlock::system(text));
    append_consumer_billing_surface(app, agent_id)
}

/// Consumer credit follow-up for `/usage` (redirect or non-silent billing fetch).
pub(super) fn append_consumer_billing_surface(app: &mut AppView, agent_id: AgentId) -> Vec<Effect> {
    if !app.usage_visible {
        return vec![];
    }
    // Remote-settings kill switch (`distill_usage_redirect_url`): link out instead of fetching billing from the backend
    if let Some(url) = app.usage_billing_redirect_url.clone() {
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            agent.scrollback.push_block(RenderBlock::System(
                crate::scrollback::blocks::SystemMessageBlock::new(format!(
                    "Please check your usage on {url}"
                )),
            ));
        }
        return vec![];
    }
    if !app.agents.contains_key(&agent_id) {
        return vec![];
    }
    // Non-silent: the effect also pulls the auto top-up rule so the summary renders usage, prepaid credits, and auto top-up together
    vec![Effect::FetchBilling {
        agent_id,
        silent: false,
        nonce: Default::default(),
    }]
}

/// `/usage manage`: open consumer billing. No-op when the surface is hidden.
pub(super) fn dispatch_manage_billing(app: &mut AppView) -> Vec<Effect> {
    if !app.usage_visible {
        return vec![];
    }
    super::router::dispatch(
        crate::app::actions::Action::OpenUrl("https://grok.com/?_s=usage".to_string()),
        app,
    )
}

/// Commit a one-line "update available" notice into the active agent's scrollback.
/// Minimal mode has no welcome screen (where the full TUI shows updates), so the background update check's result is shown here instead.
/// No-op when there is no active agent.
pub(crate) fn commit_minimal_update_notice(app: &mut AppView, latest_version: &str) {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        agent.scrollback.push_block(RenderBlock::system(format!(
            "Update available: v{latest_version}. Restart to apply."
        )));
    }
}

/// `/queue`: commit a read-only list of the queued prompts as a system block.
/// The text is built by [`crate::app::status_blocks::queue_block_text`]; this just resolves the active agent and pushes it.
/// Works in every render mode; in minimal, which has no interactive `QueuePane`, it is the primary way to inspect the queue.
pub(super) fn dispatch_show_queue(app: &mut AppView) -> Vec<Effect> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        let text = crate::app::status_blocks::queue_block_text(agent);
        agent.scrollback.push_block(RenderBlock::system(text));
    }
    vec![]
}

/// `/tasks`: commit a read-only list of background tasks, subagents, and scheduled (`/loop`) tasks as a system block.
/// The text is built by [`crate::app::status_blocks::tasks_block_text`]; this just resolves the active agent and pushes it.
/// Works in every render mode; in minimal, which has no interactive `TasksPane`, it is the primary task snapshot.
pub(super) fn dispatch_show_tasks(app: &mut AppView) -> Vec<Effect> {
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get_mut(&id)
    {
        let text = crate::app::status_blocks::tasks_block_text(agent);
        agent.scrollback.push_block(RenderBlock::system(text));
    }
    vec![]
}

/// Open the hidden `/gboom` easter egg as a modal over the active agent view.
/// Requires a graphics-capable terminal (kitty protocol or iTerm2); otherwise a toast explains why nothing happened.
/// On session-less views (dashboard, welcome) this is a silent no-op.
pub(super) fn dispatch_open_gboom(app: &mut AppView) -> Vec<Effect> {
    use crate::terminal::image::{GraphicsProtocol, detect_graphics_protocol};
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if detect_graphics_protocol() == GraphicsProtocol::None {
        agent.show_toast(
            "No demons here: GBOOM needs a graphics-capable terminal \
             (kitty, Ghostty, WezTerm, iTerm2)",
        );
        return vec![];
    }
    // Close other media modals: they share the kitty placement id
    // Drop the image viewer's in-flight loader too (its close path clears both; a leaked rx would mis-feed the next image viewer's poll loop)
    agent.image_viewer = None;
    agent.image_load_rx = None;
    agent.video_viewer = None;
    let mut game = crate::gboom::GboomState::new();
    // Deliberately `kitty_flags_pushed`, not `kitty_releases_reported`: the game pushes its own REPORT_ALL_KEYS layer over a downgraded base
    game.set_release_aware(crate::terminal::kitty_flags_pushed());
    agent.gboom = Some(game);
    vec![]
}

/// Emit a `SessionReady` notification for the given agent.
///
/// Takes `&NotificationService` separately from `&AgentView` to avoid borrow-checker conflicts when `agent` is borrowed from `app.agents`.
pub(super) fn notify_session_ready(
    notification_service: &crate::notifications::NotificationService,
    agent: &AgentView,
) {
    notification_service.notify(NotificationEvent {
        kind: NotificationEventKind::SessionReady,
        title: "Grok".into(),
        body: NotificationEventKind::SessionReady.as_ref().into(),
        session_id: agent.session.session_id.as_ref().map(|s| s.0.to_string()),
    });
}

// TaskResult handlers.

pub(super) fn handle_coding_data_sharing_updated(
    app: &mut AppView,
    agent_id: AgentId,
    opted_in: bool,
    seq: u64,
) -> Vec<Effect> {
    if !is_current_coding_data_write(app, seq, agent_id) {
        // The server accepted this older write, so the newer one falls back to it. Arrival order is not commit order: a delayed reply can overwrite a newer value here
        if let Some(pending) = app.coding_data_pending_write.as_mut() {
            pending.rollback_to_opted_in = opted_in;
        }
        return vec![];
    }
    // Re-anchor mirror to server-confirmed value (defense-in-depth against server reshaping the boolean)
    // `agent_id` discarded; privacy is app-level, not per-agent
    set_coding_data_sharing_inner(app, opted_in);
    refresh_open_settings_modals(app);
    tracing::info!(
        target: "settings",
        key = "coding_data_sharing",
        ?agent_id,
        opted_in,
        "ACP update confirmed; mirror re-anchored",
    );
    // Ack whichever way the server settled the value: the user answered and the server accepted
    if app.coding_data_pending_write.take().is_some() {
        return ack_privacy_banner(app);
    }
    vec![]
}

pub(super) fn handle_coding_data_sharing_failed(
    app: &mut AppView,
    agent_id: AgentId,
    error: String,
    seq: u64,
) -> Vec<Effect> {
    // A superseded failure must neither revert nor toast: nothing the user is looking at failed
    if !is_current_coding_data_write(app, seq, agent_id) {
        return vec![];
    }
    let rollback_to_opted_in = app
        .coding_data_pending_write
        .take()
        .map(|w| w.rollback_to_opted_in);
    if let Some(rollback) = rollback_to_opted_in {
        set_coding_data_sharing_inner(app, rollback);
    }
    refresh_open_settings_modals(app);
    let scrubbed = scrub_error_for_toast(&error);
    app.show_toast(&format!(
        "\u{2717} Couldn't update coding data sharing: {scrubbed}"
    ));
    tracing::warn!(
        target: "settings",
        key = "coding_data_sharing",
        ?agent_id,
        rollback_to_opted_in,
        %error,
        "ACP update failed",
    );
    vec![]
}

/// Stamp `[privacy].privacy_banner_acked` (in memory and on disk).
/// No-op when the notice is not rolled out: a Settings pick must not hide a notice the user has not been shown.
pub(in crate::app::dispatch) fn ack_privacy_banner(app: &mut AppView) -> Vec<Effect> {
    if !app.privacy_notice_rollout {
        return vec![];
    }
    let acked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    app.privacy_banner_acked = Some(acked_at.clone());
    vec![Effect::PersistPrivacyBannerAcked { acked_at }]
}

/// `[Opt in]`: opt in via the settings path.
/// Ack only after ACP success, so a failed round trip leaves the banner up instead of recording a change that did not happen.
pub(in crate::app::dispatch) fn dispatch_privacy_banner_opt_in(app: &mut AppView) -> Vec<Effect> {
    if app.coding_data_pending_write.is_some() || !app.privacy_banner_should_show() {
        return vec![];
    }
    set_coding_data_sharing(
        app,
        true,
        distill_telemetry::events::CodingDataConsentSource::PrivacyBanner,
    )
}

/// `[Opt out]`: always writes (the local "out" may be the unconfirmed fail-safe default) and acks only after ACP success.
pub(in crate::app::dispatch) fn dispatch_privacy_banner_opt_out(app: &mut AppView) -> Vec<Effect> {
    if app.coding_data_pending_write.is_some() || !app.privacy_banner_should_show() {
        return vec![];
    }
    set_coding_data_sharing(
        app,
        false,
        distill_telemetry::events::CodingDataConsentSource::PrivacyBanner,
    )
}

pub(super) fn handle_context_info_complete(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: &acp::SessionId,
    info: Box<distill_shell::session::SessionInfoResponse>,
    nonce: u64,
) -> Vec<Effect> {
    let minimal = app.screen_mode.is_minimal();
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        if agent.session.session_id.as_ref() != Some(session_id) {
            return vec![];
        }
        // A reply from a previous modal open must not touch anything, not even the agent's context mirrors, which a fresher reply already set
        if let Some(state) = usage_modal_state_mut(agent)
            && state.fetch_nonce != nonce
        {
            return vec![];
        }
        let model = info.data.model.as_deref().unwrap_or("unknown").to_string();
        let snapshot = info.data.context;
        agent.apply_full_context_info(snapshot.clone());
        if let Some(state) = usage_modal_state_mut(agent) {
            state.context = Some(crate::scrollback::blocks::ContextInfoBlock::new(
                snapshot, model,
            ));
            state.context_error = None;
        } else if minimal {
            push_and_page_flip(
                &mut agent.scrollback,
                crate::scrollback::block::RenderBlock::context_info(snapshot, model),
            );
        }
        // Full mode with the modal closed: the result arrived after dismissal, so drop it
    }
    vec![]
}

// Action handlers.

pub(super) fn dispatch_copy_session_id(app: &mut AppView, index: usize) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;
    // Try agent modal first, then fall back to app fields (welcome screen).
    let id = get_active_agent(app)
        .and_then(|agent| {
            if let Some(ActiveModal::SessionPicker {
                entries: Some(ref e),
                ..
            }) = agent.active_modal
            {
                e.get(index).map(|entry| entry.id.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            app.session_picker_entries
                .as_ref()
                .and_then(|s| s.get(index))
                .map(|e| e.id.clone())
        });
    if let Some(id) = id {
        let delivery = crate::clipboard::copy_text_or_file(&id);
        app.show_toast(delivery.toast_message().as_ref());
    }
    vec![]
}

/// Open the onboarding tutorial overlay (a top-level modal; works over both the welcome screen and an agent session).
/// Toggles: dispatching while open closes instead of stacking.
pub(super) fn dispatch_open_tutorial(app: &mut AppView) -> Vec<Effect> {
    // Minimal mode has no modal host: the overlay would render nothing while the app-level intercept swallowed all input
    if app.screen_mode.is_minimal() {
        return vec![];
    }
    if app.tutorial.is_some() {
        app.tutorial = None;
        return vec![];
    }
    app.tutorial = Some(crate::views::tutorial::TutorialState::new());
    vec![]
}

pub(super) fn dispatch_show_release_notes(
    app: &mut AppView,
    title: String,
    content: String,
) -> Vec<Effect> {
    match app.active_view {
        ActiveView::Agent(id) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.active_modal = Some(crate::views::modal::ActiveModal::DocViewer {
                    title,
                    content,
                    scroll: 0,
                    window: crate::views::modal_window::ModalWindowState::new(),
                    cached_lines: None,
                    previous_palette: None,
                    standalone: true,
                    tier_editor: None,
                });
            }
        }
        ActiveView::Welcome => {
            app.welcome_doc_viewer = Some(crate::views::modal::ActiveModal::DocViewer {
                title,
                content,
                scroll: 0,
                window: crate::views::modal_window::ModalWindowState::new(),
                cached_lines: None,
                previous_palette: None,
                standalone: true,
                tier_editor: None,
            });
        }
        _ => {}
    }
    vec![]
}

/// Open the editable model tier form on whichever screen owns the prompt.
pub(super) fn dispatch_show_tier_editor(app: &mut AppView) -> Vec<Effect> {
    let make_modal =
        |models: &crate::acp::ModelState| crate::views::modal::ActiveModal::DocViewer {
            title: "Model tiers".to_owned(),
            content: crate::slash::commands::provider_status::tier_status(
                models.current_model_id_str(),
            ),
            scroll: 0,
            window: crate::views::modal_window::ModalWindowState::new(),
            cached_lines: None,
            previous_palette: None,
            standalone: true,
            tier_editor: Some(Box::new(
                crate::views::tier_editor::TierEditorState::from_models(models),
            )),
        };

    match app.active_view {
        ActiveView::Agent(id) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.active_modal = Some(make_modal(&agent.session.models));
            }
        }
        ActiveView::Welcome => {
            app.welcome_doc_viewer = Some(make_modal(&app.models));
        }
        _ => {}
    }
    vec![]
}
