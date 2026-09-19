// Modified for Distill by Samuel Fajreldines, 2026.
//! Feedback, remember-note, btw, and recap dispatchers.

use super::ctx::{NO_SESSION_NOTICE, with_active_agent};
use agent_client_protocol as acp;

use crate::app::actions::{Effect, FeedbackSendOrigin, FeedbackTraceChoice};
use crate::app::agent::AgentId;
use crate::app::agent_view::{AgentView, PromptInputMode};
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::{SessionEvent, ToolCallBlock};
use distill_feedback::{
    FeedbackDraftStore, FeedbackSource, FeedbackTaxonomy, derive_title, structured_feedback,
};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter for correlating async rewrite responses with the modal that requested them.
/// It prevents stale results from populating a different note's review modal when the user closes and re-opens quickly.
static REWRITE_NONCE: AtomicU64 = AtomicU64::new(0);

fn next_rewrite_nonce() -> u64 {
    REWRITE_NONCE.fetch_add(1, Ordering::Relaxed)
}

/// One copy of the send-time thank-you, shared by the immediate and modal commit paths.
pub(crate) const FEEDBACK_THANKS_NOTICE: &str =
    "Thanks for the feedback! The Distill team is on it.";

/// Minimal mode cannot show a toast, so the notice goes to the transcript instead.
fn feedback_notice(app: &mut AppView, message: &str) {
    if app.screen_mode.is_minimal() {
        with_active_agent(app, |agent| {
            agent
                .scrollback
                .push_block(RenderBlock::system(message.to_string()));
        });
    } else {
        app.show_toast(message);
    }
}

/// Open the feedback modal (every screen mode; minimal hosts it in its live band).
/// Every refusal is visible (voice notice, blocker notice, or no-session notice); the state is never set invisibly.
/// Early exits drop `open`, whose image owner cleans up the staged temp files.
pub(super) fn dispatch_open_feedback_modal(
    app: &mut AppView,
    open: crate::views::feedback_modal::OpenFeedbackModal,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        if matches!(app.active_view, ActiveView::AgentDashboard)
            && let Some(dashboard) = app.dashboard.as_mut()
        {
            dashboard.dispatch.set_text("");
            dashboard.set_error_toast(NO_SESSION_NOTICE);
        }
        return vec![];
    };
    if matches!(
        app.voice_recording_target(),
        Some(crate::app::app_view::VoiceTarget::Agent(target)) if target == id
    ) {
        feedback_notice(app, "Stop voice input before opening the feedback form");
        return vec![];
    }
    let blocked = {
        let Some(agent) = app.agents.get(&id) else {
            return vec![];
        };
        agent.feedback_modal_open_blocker().or_else(|| {
            agent
                .session
                .session_id
                .is_none()
                .then_some(NO_SESSION_NOTICE)
        })
    };
    if let Some(message) = blocked {
        feedback_notice(app, message);
        return vec![];
    }
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let prefill_chars = open.text.as_deref().map_or(0, |t| t.chars().count());
    let prefill_images = open.images.len();
    crate::unified_log::info(
        "feedback.modal_open",
        agent.session.session_id.as_ref().map(|s| s.0.as_ref()),
        Some(serde_json::json!({
            "prefill_chars": prefill_chars,
            "prefill_images": prefill_images,
            // Fixed taxonomy labels only, never user-authored text.
            "type": open.r#type.as_ref().map(crate::views::feedback_modal::FeedbackType::label),
            "task_category": open
                .task_category
                .as_ref()
                .map(crate::views::feedback_modal::FeedbackTaskCategory::label),
            "failure_mode": open
                .failure_mode
                .as_ref()
                .map(crate::views::feedback_modal::FeedbackFailureMode::label),
            "has_draft_id": open.draft_id.is_some(),
        })),
    );
    let draft_id = open.draft_id.clone();
    let modal = crate::views::feedback_modal::FeedbackModalState::new(open);
    let modal_id = modal.id();
    let rehydrations = modal.image_rehydration_requests();
    agent.feedback_modal = Some(modal);
    let mut effects = rehydrations
        .into_iter()
        .map(|(image_identity, path)| Effect::RehydrateFeedbackImage {
            agent_id: id,
            modal_id,
            image_identity,
            path,
        })
        .collect::<Vec<_>>();
    if draft_id.is_none()
        && let Some(modal) = agent.feedback_modal.as_mut()
    {
        modal.start_open_draft_list();
        if let Some(request) = modal.take_pending_request()
            && let Some(session_id) = agent.session.session_id.clone()
        {
            effects.push(Effect::FeedbackDraftRequest {
                agent_id: id,
                session_id,
                request,
            });
        }
    }
    if let Some(draft_id) = draft_id
        && let Some(modal) = agent.feedback_modal.as_mut()
    {
        modal.start_external_draft_load(draft_id);
        if let Some(request) = modal.take_pending_request() {
            let Some(session_id) = agent.session.session_id.clone() else {
                return effects;
            };
            effects.push(Effect::FeedbackDraftRequest {
                agent_id: id,
                session_id,
                request,
            });
        }
    }
    effects
}

/// (when the offer applies and no choice was made yet) or encode the images, close the modal, thank immediately, and emit only the POST. The one-shot upload is never a sibling of the POST:
/// "Never submit while paste probes are in flight" is owned by the modal's key layer
/// (`FeedbackModalState::handle_key` defers behind the probe count); every producer of `Action::SubmitFeedbackModal` must route through it or `take_deferred_submit`.
pub(super) fn dispatch_submit_feedback_modal(
    app: &mut AppView,
    modal_id: crate::views::feedback_modal::FeedbackModalId,
) -> Vec<Effect> {
    use crate::views::feedback_modal::{
        FeedbackTraceChoice as ModalTraceChoice, FeedbackTraceUploadIntent,
    };

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let offer_trace = app.feedback_trace_offer()
        && !app.feedback_trace_choice_latched
        && !app.coding_data_retention_opt_out
        && app.coding_data_sharing_lock().is_none()
        && app.team_name.is_none()
        && !app.is_zdr
        && !app.screen_mode.is_minimal();
    // Modal copy never discloses re-enabling coding-data sharing (one archive, this report only).
    // Unlike the legacy AlwaysUpload card, this path does not call `set_coding_data_sharing`.
    let trace_reenables_sharing = false;
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let session_id = agent.session.session_id.clone();
    let has_trace_capacity = agent.has_feedback_trace_capacity();
    let Some(modal) = agent.feedback_modal.as_mut() else {
        return vec![];
    };
    // A stale submit (deferred behind a paste, or replayed) must never send a different modal's
    // draft; a duplicate after the send-time close finds no modal at all and already returned.
    if !modal.matches_id(modal_id) {
        return vec![];
    }
    // Only an Enter-confirmed trace choice may send; a stray submit while the user is still choosing is a no-op.
    let trace_choice = if modal.in_trace_step() {
        let Some(choice) = modal.decided_trace_choice() else {
            return vec![];
        };
        Some(choice)
    } else {
        None
    };
    if trace_choice == Some(ModalTraceChoice::SendThisSession) && !has_trace_capacity {
        modal.set_error(
            "Wait for an earlier feedback trace upload to finish, then try again.".to_string(),
        );
        return vec![];
    }
    let Some(session_id) = session_id else {
        modal.set_error(NO_SESSION_NOTICE.to_string());
        return vec![];
    };
    if !modal.is_sendable() {
        modal.set_error(crate::views::feedback_modal::FEEDBACK_EMPTY_SUBMIT_ERROR.to_string());
        return vec![];
    }
    let text = modal.submitted_text().trim().to_string();
    modal.reconcile_feedback_images();
    let (encoded_images, dropped) = encode_feedback_image_slice(modal.images());
    if text.is_empty() && encoded_images.is_empty() {
        if let Some(notice) = dropped {
            modal.set_error(notice);
        } else {
            modal.set_error(crate::views::feedback_modal::FEEDBACK_EMPTY_SUBMIT_ERROR.to_string());
        }
        return vec![];
    }
    // Refuse a typeless loaded draft before the in-modal trace step, or the choose-a-type error is hidden until after the user confirms a trace choice.
    // choose-a-type error is hidden until after the user confirms a trace choice.
    let draft_id = modal.draft_id().cloned();
    let draft_fields = modal.draft_body();
    if draft_id.is_some() && draft_fields.is_none() {
        modal.cancel_draft_submit_pending("Choose a type before sending this draft.".to_owned());
        return vec![];
    }
    // First validated Write submit with an offer available: ask inside the modal instead of sending.
    if trace_choice.is_none() && offer_trace {
        modal.begin_trace_step();
        // Funnel denominator for the in-modal trace step; logged once when Write actually advances.
        distill_telemetry::session_ctx::log_event(
            distill_telemetry::events::FeedbackTraceCardShown {
                reenables_sharing: trace_reenables_sharing,
            },
        );
        return vec![];
    }
    let draft = draft_id
        .clone()
        .zip(draft_fields)
        .map(
            |(draft_id, fields)| crate::app::actions::DraftFeedbackBody {
                draft_id,
                title: fields.title,
                details: text.clone(),
                area: fields.area,
                r#type: fields.r#type,
                task_category: fields.task_category,
                failure_mode: fields.failure_mode,
                images: encoded_images.clone(),
            },
        );
    if let Some(choice) = trace_choice {
        modal.report_confirmed_trace_choice(choice);
        let _ = modal.take_decided_trace_choice();
    }
    let metadata = Some(modal.structured_feedback_metadata());
    if draft_id.is_none() {
        drop(modal.take_images());
    }
    // `send_this_session` is one archive for this successfully posted report; every other
    // choice parks nothing, so no path from this modal can persist `[telemetry] trace_upload`.
    let intent = match trace_choice {
        Some(ModalTraceChoice::SendThisSession) => Some(FeedbackTraceUploadIntent::SendThisSession),
        _ => None,
    };
    let logged_trace = trace_choice.map(|choice| format!("{choice:?}"));
    let submission_id = crate::views::feedback_modal::FeedbackSubmissionId::next();
    if draft_id.is_some() {
        modal.mark_draft_submit_pending();
    } else {
        agent.feedback_modal = None;
    }
    if let Some(intent) = intent {
        agent.park_feedback_trace_consent(
            submission_id,
            crate::views::feedback_modal::ParkedFeedbackTraceConsent {
                intent,
                session_id: session_id.0.as_ref().to_string(),
            },
        );
    }
    if let Some(notice) = dropped {
        agent.scrollback.push_block(RenderBlock::system(notice));
    }
    if draft_id.is_none() {
        agent
            .scrollback
            .push_block(RenderBlock::system(FEEDBACK_THANKS_NOTICE.to_string()));
    }
    let mut effects = vec![feedback_send_effect(
        id,
        session_id,
        text,
        encoded_images,
        /*trace*/ None,
        logged_trace,
        metadata,
        /* request_trace_upload_token */ intent.is_some(),
        draft,
        FeedbackSendOrigin::Modal {
            submission_id,
            modal_id,
            is_draft: draft_id.is_some(),
        },
    )];
    if trace_choice == Some(ModalTraceChoice::NeverAsk) {
        // Stop re-offers this session and persist the existing feature latch; never `[telemetry] trace_upload = true`.
        app.feedback_trace_choice_latched = true;
        effects.push(Effect::PersistSetting {
            key: "feedback_trace_card",
            value: crate::settings::SettingValue::Bool(false),
            rollback_value: crate::settings::SettingValue::Bool(false),
        });
    }
    effects
}

/// How long the background trace upload may run before it is reported as failed; longer than the shell's own upload timeout so its error wins.
pub(crate) const FEEDBACK_TRACE_UPLOAD_TIMEOUT_MS: u64 = 150_000;

/// Enter remember mode: visual change to prompt bar (remember accent, `#` prefix).
/// No side effects: the user types a memory note and presses Enter to send.
pub(super) fn dispatch_enter_remember_mode(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        agent.prompt_input_mode = PromptInputMode::Remember;
        agent.prompt.set_text("");
    });
    vec![]
}

/// Log the trace-consent outcome carried on an immediate send exactly once.
pub(crate) fn log_trace_consent_selected(reenables_sharing: bool, choice: FeedbackTraceChoice) {
    use distill_telemetry::events::{FeedbackTraceConsentChoice, FeedbackTraceConsentSelected};
    distill_telemetry::session_ctx::log_event(FeedbackTraceConsentSelected {
        choice: match choice {
            FeedbackTraceChoice::AlwaysUpload => FeedbackTraceConsentChoice::TurnOn,
            FeedbackTraceChoice::NeverAsk => FeedbackTraceConsentChoice::NeverAsk,
            FeedbackTraceChoice::NoUpload => FeedbackTraceConsentChoice::NoUpload,
        },
        reenables_sharing,
    });
}
/// The `feedback.send` unified log plus the POST effect for a committed report.
/// This is the single writer for both, shared with the modal submit path.
pub(crate) fn feedback_send_effect(
    agent_id: AgentId,
    session_id: agent_client_protocol::SessionId,
    text: String,
    images: Vec<distill_shell::session::FeedbackImage>,
    trace: Option<FeedbackTraceChoice>,
    trace_log: Option<String>,
    metadata: Option<serde_json::Value>,
    request_trace_upload_token: bool,
    draft: Option<crate::app::actions::DraftFeedbackBody>,
    origin: FeedbackSendOrigin,
) -> Effect {
    let mut payload = serde_json::json!({
        "chars": text.chars().count(),
        "images": images.len(),
        "trace": trace_log.unwrap_or_else(|| match trace {
            Some(choice) => format!("{choice:?}"),
            None => "NotOffered".to_string(),
        }),
    });
    if matches!(origin, FeedbackSendOrigin::Modal { .. })
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("modal".into(), serde_json::Value::Bool(true));
    }
    crate::unified_log::info("feedback.send", Some(session_id.0.as_ref()), Some(payload));
    Effect::SendFeedback {
        agent_id,
        session_id,
        feedback_text: text,
        images,
        metadata,
        request_trace_upload_token,
        draft,
        origin,
    }
}

/// Commit a report: encode images (with the dropped-attachment notice), close the consent funnel, and build the send effect.
/// The "no text and no surviving images means do not send" rule lives only here.
pub(crate) fn commit_feedback(
    agent: &mut crate::app::agent_view::AgentView,
    coding_data_retention_opt_out: bool,
    id: AgentId,
    session_id: agent_client_protocol::SessionId,
    text: String,
    images: crate::views::prompt_widget::FeedbackImages,
    trace: Option<FeedbackTraceChoice>,
) -> Option<Effect> {
    // Encode before the emptiness check: encoding can drop attachments, and a report left with no text and no images must not go out blank
    let (encoded_images, dropped) = encode_feedback_image_slice(images.as_slice());
    // Encoding is done with the records; dropping the owner deletes the staged temp files
    drop(images);
    if let Some(notice) = dropped {
        agent.scrollback.push_block(RenderBlock::system(notice));
    }

    // A collected consent closes its funnel exactly once, sent or not.
    if let Some(choice) = trace {
        log_trace_consent_selected(coding_data_retention_opt_out, choice);
    }

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() && encoded_images.is_empty() {
        agent.scrollback.push_block(RenderBlock::system(
            "Please provide feedback text.".to_string(),
        ));
        return None;
    }

    agent
        .scrollback
        .push_block(RenderBlock::system(FEEDBACK_THANKS_NOTICE.to_string()));

    Some(feedback_send_effect(
        id,
        session_id,
        trimmed,
        encoded_images,
        trace,
        /*trace_log*/ None,
        // Inline text is composed like a Write-tab report, minus the taxonomy enums.
        Some(structured_feedback(
            FeedbackSource::Write,
            FeedbackTaxonomy::default(),
        )),
        /* request_trace_upload_token */ false,
        /*draft*/ None,
        FeedbackSendOrigin::Immediate,
    ))
}

/// A report that did not reach the backend.
pub(super) struct UnsentFeedbackReport<'a> {
    pub(super) text: &'a str,
    /// Attachments are not carried into the draft, so the notice says how many were lost.
    pub(super) image_count: usize,
    /// Leading clause of the notice, e.g. "Couldn't send feedback: offline".
    pub(super) failure: &'a str,
}

/// The composer was cleared at send time, so the predraft written here is the only copy left;
/// without a session directory the text is echoed instead. The store write is sync and on the
/// failure path only: the effect task has no session dir to write from.
pub(super) fn keep_unsent_feedback_report(agent: &mut AgentView, report: UnsentFeedbackReport<'_>) {
    let UnsentFeedbackReport {
        text,
        image_count,
        failure,
    } = report;
    let attachments = match image_count {
        0 => String::new(),
        1 => " (its image was dropped)".to_owned(),
        count => format!(" (its {count} images were dropped)"),
    };
    // An image-only report has nothing the store could keep.
    if text.trim().is_empty() {
        agent.scrollback.push_block(RenderBlock::system(format!(
            "{failure}. Not sent{attachments}."
        )));
        return;
    }
    let saved = agent.session.ensure_local_session_dir().map(|session_dir| {
        let session_dir = session_dir.map_err(|error| error.to_string())?;
        FeedbackDraftStore::new(session_dir)
            .append_predraft(&derive_title(text), text)
            .map_err(|error| error.to_string())
    });
    let notice = match saved {
        Some(Ok(_)) => {
            format!("{failure}. Saved to Drafts{attachments}; open `/feedback` to retry.")
        }
        Some(Err(error)) => {
            format!(
                "{failure}. Could not save it as a draft ({error}). Not sent{attachments}: {text}"
            )
        }
        None => format!("{failure}. Not sent{attachments}: {text}"),
    };
    agent.scrollback.push_block(RenderBlock::system(notice));
}

/// Thank-you is shown immediately; POST is a background effect.
/// The composer is not cleared: the text arrives with the action, not from the prompt.
/// Early exits drop `images`, whose owner cleans up the staged temp files.
pub(super) fn dispatch_send_feedback(
    app: &mut AppView,
    text: String,
    images: crate::views::prompt_widget::FeedbackImages,
    trace: Option<FeedbackTraceChoice>,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let coding_data_retention_opt_out = app.coding_data_retention_opt_out;
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    agent.ephemeral_tip.clear_on_submit();

    let Some(session_id) = agent.session.session_id.clone() else {
        // The slash path already cleared the composer, so the report has nowhere else to live.
        keep_unsent_feedback_report(
            agent,
            UnsentFeedbackReport {
                text: text.trim(),
                image_count: images.len(),
                failure: NO_SESSION_NOTICE,
            },
        );
        return vec![];
    };

    let Some(send) = commit_feedback(
        agent,
        coding_data_retention_opt_out,
        id,
        session_id.clone(),
        text,
        images,
        trace,
    ) else {
        // Nothing went out, so no trace-upload side effects either.
        return vec![];
    };

    let mut effects = vec![send];
    match trace {
        None | Some(FeedbackTraceChoice::NoUpload) => {}
        Some(FeedbackTraceChoice::NeverAsk) => {
            app.feedback_trace_choice_latched = true;
            effects.push(Effect::PersistSetting {
                key: "feedback_trace_card",
                value: crate::settings::SettingValue::Bool(false),
                rollback_value: crate::settings::SettingValue::Bool(false),
            });
        }
        Some(FeedbackTraceChoice::AlwaysUpload) => {
            app.feedback_trace_choice_latched = true;
            if app.coding_data_retention_opt_out {
                effects.extend(super::status::set_coding_data_sharing(
                    app,
                    true,
                    distill_telemetry::events::CodingDataConsentSource::FeedbackTraceCard,
                ));
            }
            effects.push(Effect::UploadFeedbackTrace {
                agent_id: id,
                session_id,
                submission_id: None,
                intent: None,
                trace_upload_token: None,
            });
            // The `[telemetry] trace_upload = true` write an `AlwaysUpload` consent collects.
            effects.push(Effect::PersistSetting {
                key: "trace_upload",
                value: crate::settings::SettingValue::Bool(true),
                rollback_value: crate::settings::SettingValue::Bool(false),
            });
        }
    }
    effects
}

/// The `#` composer path. Nothing else records the note, so this records it.
pub(super) fn dispatch_send_remember_note(app: &mut AppView, text: String) -> Vec<Effect> {
    send_remember_note(app, text, true)
}

/// The `/remember <text>` path. `dispatch_send_prompt_inner` already recorded the typed command.
pub(super) fn dispatch_send_remember_note_from_command(
    app: &mut AppView,
    text: String,
) -> Vec<Effect> {
    send_remember_note(app, text, false)
}

/// Encode a borrowed image snapshot for the POST.
fn encode_feedback_image_slice(
    images: &[crate::prompt_images::PastedImage],
) -> (Vec<distill_shell::session::FeedbackImage>, Option<String>) {
    use base64::Engine as _;

    let loaded: Vec<Option<(Vec<u8>, String)>> = images
        .iter()
        .map(crate::prompt_images::load_for_send)
        .collect();
    let (accepted, notice) = super::inline_feedback::select_feedback_images(&loaded);
    let encoded = accepted
        .into_iter()
        .filter_map(|index| {
            let (bytes, mime_type) = loaded.get(index).and_then(Option::as_ref)?;
            Some(distill_shell::session::FeedbackImage {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                mime_type: mime_type.clone(),
                file_name: images
                    .get(index)
                    .and_then(|img| img.source_path.as_deref())
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned()),
            })
        })
        .collect();
    (encoded, notice)
}

/// Send a raw remember note for LLM-powered rewriting via `x.ai/memory/rewrite`.
/// Clears remember mode and prompts the LLM to reformat the note with session context.
/// Falls back to direct `SaveMemoryNote` when no session is available.
fn send_remember_note(app: &mut AppView, text: String, record_in_history: bool) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    agent.prompt_input_mode = PromptInputMode::Normal;
    agent.prompt.set_text("");
    agent.ephemeral_tip.clear_on_submit();

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        agent.scrollback.push_block(RenderBlock::system(
            "Please provide a memory note.".to_string(),
        ));
        return vec![];
    }

    agent.note_draft_consumed();
    if record_in_history {
        // The note is stored without the `#`; recall decodes a prefix back into its mode, which would turn `# Context` into a note
        agent.record_prompt_in_history(&trimmed);
    }

    let cwd = agent.session.cwd.clone();

    let Some(session_id) = agent.session.session_id.clone() else {
        // No session: open the modal with raw content only (no LLM rewrite)
        agent.active_modal = Some(ActiveModal::RememberNoteReview {
            raw_content: trimmed.clone(),
            enhanced_content: None, // No session, so no LLM rewrite and Tab is disabled
            showing_enhanced: false,
            scroll: 0,
            window: crate::views::modal_window::ModalWindowState::new(),
            cached_lines: None,
            cwd,
            agent_id: id,
            rewrite_nonce: Default::default(), // no rewrite in flight, nonce unused
        });
        return vec![];
    };

    // Open modal with raw content, LLM rewrite in flight.
    let nonce = next_rewrite_nonce();
    agent.active_modal = Some(ActiveModal::RememberNoteReview {
        raw_content: trimmed.clone(),
        enhanced_content: None,
        showing_enhanced: false,
        scroll: 0,
        window: crate::views::modal_window::ModalWindowState::new(),
        cached_lines: None,
        cwd: cwd.clone(),
        agent_id: id,
        rewrite_nonce: nonce,
    });

    let context_summary = extract_session_context(agent);

    vec![Effect::RewriteMemoryNote {
        agent_id: id,
        session_id,
        raw_text: trimmed,
        context_summary,
        nonce,
    }]
}

/// Save the currently displayed remember note from the review modal.
pub(super) fn dispatch_save_remember_note_from_modal(app: &mut AppView) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    let (content, cwd) = if let Some(ActiveModal::RememberNoteReview {
        ref raw_content,
        ref enhanced_content,
        showing_enhanced,
        ref cwd,
        ..
    }) = agent.active_modal
    {
        let text = if showing_enhanced {
            enhanced_content.as_deref().unwrap_or(raw_content)
        } else {
            raw_content
        };
        (text.trim().to_string(), cwd.clone())
    } else {
        return vec![];
    };
    let pinned_mode = agent
        .session
        .session_id
        .as_ref()
        .map(|_| agent.memory_mode.unwrap_or_default());

    agent.active_modal = None;
    agent
        .scrollback
        .push_block(RenderBlock::system("Saving memory note...".to_string()));

    vec![Effect::SaveMemoryNote {
        agent_id: id,
        text: content,
        cwd,
        pinned_mode,
    }]
}

/// Extract session context for the LLM memory rewrite request.
/// File paths from recent tool calls (Read, Edit, ListDir)
fn extract_session_context(agent: &AgentView) -> String {
    let mut user_prompts: Vec<String> = Vec::new();
    let mut file_paths: Vec<String> = Vec::new();

    // Walk scrollback entries in reverse to collect recent context.
    let len = agent.scrollback.len();
    for i in (0..len).rev() {
        let Some(entry) = agent.scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            RenderBlock::UserPrompt(prompt) => {
                if user_prompts.len() < 5 {
                    let text = if prompt.text.len() > 200 {
                        let end = prompt
                            .text
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i <= 200)
                            .last()
                            .unwrap_or(0);
                        match prompt.text.get(..end) {
                            Some(prefix) => format!("{prefix}..."),
                            None => prompt.text.clone(),
                        }
                    } else {
                        prompt.text.clone()
                    };
                    user_prompts.push(text);
                }
            }
            RenderBlock::ToolCall(tc) => {
                if file_paths.len() < 20 {
                    match tc {
                        ToolCallBlock::Read(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::Edit(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::ListDir(b) => {
                            file_paths.push(b.path.clone());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if user_prompts.len() >= 5 && file_paths.len() >= 20 {
            break;
        }
    }

    let mut parts: Vec<String> = Vec::new();

    // CWD
    parts.push(format!("CWD: {}", agent.session.cwd.display()));

    // Git branch
    if let Some(ref branch) = agent.current_branch {
        parts.push(format!("Branch: {branch}"));
    }

    // Recent prompts (chronological order)
    if !user_prompts.is_empty() {
        user_prompts.reverse();
        parts.push("Recent prompts:".to_string());
        for p in &user_prompts {
            parts.push(format!("- {p}"));
        }
    }

    // Recent file paths (deduplicated, preserving first-seen order)
    if !file_paths.is_empty() {
        let mut seen = std::collections::HashSet::new();
        file_paths.retain(|p| seen.insert(p.clone()));
        parts.push("Recent files:".to_string());
        for p in &file_paths {
            parts.push(format!("- {p}"));
        }
    }

    parts.join("\n")
}

/// Send a /btw side question.
/// Bypasses the prompt queue, so it works even while the agent is mid-turn.
/// Fires an ACP ext method and shows a loading overlay.
pub(super) fn dispatch_send_btw(
    app: &mut AppView,
    question: String,
    images: Vec<crate::prompt_images::PastedImage>,
) -> Vec<Effect> {
    // Drop deletes staged and session files. A side question does not keep them.
    let images = BtwImages::new(images);
    // Slash submit strips image chips before the command runs. Put `[Image]`
    // back so the panel title shows the attachment.
    let question = question_with_image_chips(&question, images.as_slice());
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let minimal = app.screen_mode.is_minimal();
    let (session_id, minimal_request_id, cwd) = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        let Some(session_id) = agent.session.session_id.clone() else {
            if minimal {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(
                        NO_SESSION_NOTICE,
                    ));
            } else {
                agent.show_toast(NO_SESSION_NOTICE);
            }
            return vec![];
        };
        let cwd = std::path::PathBuf::from(&agent.session.cwd);

        // Composer clearing belongs to the submit funnel: `dispatch_send_prompt_inner` clears it when `consume_input` is set
        // Draft-preserving callers (the palette, an edited queue row) keep theirs
        let minimal_request_id = if minimal {
            Some(crate::minimal_api::start_minimal_btw(
                agent,
                question.clone(),
            ))
        } else {
            agent.clear_btw_owned_selection();
            agent.btw_state = Some(crate::views::btw_overlay::BtwOverlayState::Loading {
                question: question.clone(),
            });
            // Prompt keeps focus while the answer is in flight (panel focuses on Done).
            agent.btw_focused = false;
            None
        };
        (session_id, minimal_request_id, cwd)
    };

    vec![Effect::SendBtw {
        agent_id: id,
        session_id,
        question,
        images: images.into_inner(),
        cwd,
        minimal_request_id,
    }]
}

/// Owns `/btw` attachments until they are handed to the send effect.
/// Drop unlinks staged temps and session copies.
struct BtwImages(Vec<crate::prompt_images::PastedImage>);

impl BtwImages {
    fn new(images: Vec<crate::prompt_images::PastedImage>) -> Self {
        Self(images)
    }

    fn as_slice(&self) -> &[crate::prompt_images::PastedImage] {
        &self.0
    }

    fn into_inner(mut self) -> Vec<crate::prompt_images::PastedImage> {
        std::mem::take(&mut self.0)
    }
}

/// Prefix a single `[Image]` chip. The `/btw` title does not number attachments.
/// Text-only questions are returned unchanged so newlines survive.
fn question_with_image_chips(
    question: &str,
    images: &[crate::prompt_images::PastedImage],
) -> String {
    if images.is_empty() {
        return question.to_string();
    }
    let question = strip_numbered_image_chips(question.trim());
    if question.contains("[Image]") {
        return question;
    }
    if question.is_empty() {
        "[Image]".to_string()
    } else {
        format!("[Image] {question}")
    }
}

/// Remove `[Image #N]` tokens without collapsing newlines.
fn strip_numbered_image_chips(question: &str) -> String {
    let mut out = String::new();
    let mut rest = question;
    while let Some(start) = rest.find("[Image #") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "[Image #".len()..];
        let Some(end) = after.find(']') else {
            out.push_str(&rest[start..]);
            return out;
        };
        rest = &after[end + 1..];
        if out.ends_with(' ') && rest.starts_with(' ') {
            rest = rest.trim_start_matches(' ');
        }
    }
    out.push_str(rest);
    out
}

impl Drop for BtwImages {
    fn drop(&mut self) {
        crate::prompt_images::drain_and_cleanup(
            crate::prompt_images::SessionPathPolicy::Delete,
            &mut self.0,
        );
    }
}

/// Side-question payload cap. Matches the per-image send limit so one screenshot
/// under 50MB is never dropped; extra images past this total are omitted.
const BTW_IMAGE_AGGREGATE_CAP: usize = 50_000_000;

pub(crate) struct BtwImageEncode {
    pub blocks: Option<Vec<acp::ContentBlock>>,
    pub omitted: usize,
}

/// Encode composer images into `x.ai/btw` content blocks.
/// `blocks: None` keeps the text-only wire. `omitted` is how many attachments did not encode.
/// Caller unlinks the image files after this returns.
pub(crate) fn encode_btw_images(
    question: &str,
    images: &[crate::prompt_images::PastedImage],
    cwd: &std::path::Path,
) -> BtwImageEncode {
    if images.is_empty() {
        return BtwImageEncode {
            blocks: None,
            omitted: 0,
        };
    }
    let kept = images_within_aggregate_cap(images);
    let omitted_by_cap = images.len() - kept.len();
    if kept.is_empty() {
        return BtwImageEncode {
            blocks: None,
            omitted: images.len(),
        };
    }
    let blocks = crate::prompt_images::build_content_blocks_with_workspace_ref(
        question.to_string(),
        &kept,
        Some(cwd),
    );
    let produced = blocks
        .iter()
        .filter(|block| matches!(block, acp::ContentBlock::Image(_)))
        .count();
    let omitted = omitted_by_cap + kept.len().saturating_sub(produced);
    BtwImageEncode {
        blocks: (produced > 0).then_some(blocks),
        omitted,
    }
}

fn images_within_aggregate_cap(
    images: &[crate::prompt_images::PastedImage],
) -> Vec<crate::prompt_images::PastedImage> {
    let mut kept = Vec::new();
    let mut total = 0usize;
    for image in images {
        let size = image.byte_len;
        if total.saturating_add(size) > BTW_IMAGE_AGGREGATE_CAP {
            tracing::warn!(
                bytes = size,
                total,
                cap = BTW_IMAGE_AGGREGATE_CAP,
                "skipping /btw image: aggregate payload cap"
            );
            continue;
        }
        total += size;
        kept.push(image.clone());
    }
    kept
}

/// Toast when a manual `/recap` produces no summary.
/// Empty sessions get a clear empty-state message; anything else (model failure, empty summary, etc.) keeps the generic failure toast.
pub(crate) fn recap_unavailable_toast(has_user_messages: bool) -> &'static str {
    if has_user_messages {
        "Couldn't generate recap"
    } else {
        "No messages yet"
    }
}

/// Whether scrollback already has a user prompt.
/// Scans entries rather than `turn_count` so it stays correct during `begin_batch`/`end_batch` session load.
/// There `push` defers `rebuild_turns`, so `turn_count` can stay 0 while replayed prompts are already present.
pub(crate) fn scrollback_has_user_messages(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    scrollback
        .iter_entries()
        .any(|(_, entry)| entry.block.is_user_prompt())
}

/// Request a session recap.
/// Bypasses the prompt queue, so it works even while the agent is mid-turn.
/// The auto path is best-effort and silently no-ops without an active session.
pub(super) fn dispatch_send_recap(app: &mut AppView, auto: bool) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    // The shell is authoritative (remote settings, config, env)
    // Skip client requests entirely when the feature is off so we never hit `x.ai/recap`
    if !app.session_recap_available {
        if !auto {
            agent.show_toast("Session recap is not enabled");
        }
        return vec![];
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        if !auto {
            agent.show_toast(NO_SESSION_NOTICE);
        }
        return vec![];
    };

    if !auto {
        agent.prompt.set_text("");
        // Nothing to summarize yet: show a clear empty-state toast instead of a spinner that ends in "Couldn't generate recap"
        // Skip the short-circuit while session replay is still loading (prompts may not have arrived yet)
        // Prefer an entry scan over `turn_count()` so mid-batch resume (deferred `rebuild_turns`) still sees history
        if !agent.session.loading_replay && !scrollback_has_user_messages(&agent.scrollback) {
            agent.show_toast(recap_unavailable_toast(false));
            return vec![];
        }
        // Show an immediate loading block with the animated "running" sidebar so the user has feedback that a recap is being generated
        // The `SessionRecap` handler fills this entry in and stops the animation
        // Reuse an existing in-flight loading block instead of stacking spinners when `/recap` is pressed repeatedly
        let already_loading = agent.pending_recap_entry.is_some_and(|eid| {
            agent
                .scrollback
                .get_by_id(eid)
                .is_some_and(|entry| entry.is_running)
        });
        if !already_loading {
            let entry_id =
                agent
                    .scrollback
                    .push(crate::scrollback::entry::ScrollbackEntry::running(
                        RenderBlock::session_event(SessionEvent::Recap {
                            summary: String::new(),
                            auto: false,
                        }),
                    ));
            agent.pending_recap_entry = Some(entry_id);
        }
    } else {
        // Retry backoff only: do not consume the away period on dispatch
        // The shell often no-ops auto recap until at least 3 min since the last main turn
        // mark_recap_shown runs when any SessionRecap arrives (auto or manual `/recap`)
        app.notification_service
            .focus_tracker
            .note_auto_recap_attempt();
    }

    vec![Effect::SendRecap { session_id, auto }]
}

// TaskResult handlers.

pub(super) fn handle_memory_note_saved(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<(), String>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        match result {
            Ok(()) => {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(
                        "Memory note saved".to_string(),
                    ));
            }
            Err(error) => {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't save memory note: {error}"
                    )));
            }
        }
    }
    vec![]
}

pub(super) fn handle_btw_response(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<String, String>,
    minimal_request_id: Option<uuid::Uuid>,
    image_notice: Option<String>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        use crate::views::btw_overlay::BtwOverlayState;
        if let Some(request_id) = minimal_request_id {
            // A dismissed or stale request ignores the answer. Don't toast either.
            if crate::minimal_api::finish_minimal_btw(agent, request_id, result)
                && let Some(notice) = image_notice.as_deref()
            {
                agent.show_toast(notice);
            }
            return vec![];
        }
        if let Some(notice) = image_notice.as_deref() {
            agent.show_toast(notice);
        }
        let question = match &agent.btw_state {
            Some(BtwOverlayState::Loading { question }) => question.clone(),
            _ => String::new(),
        };
        match result {
            Ok(response) => {
                // Answer arrived: show it (until Esc) and focus the panel so Up/Down scroll it until the user returns to the prompt
                agent.btw_state = Some(BtwOverlayState::done(question, response));
                agent.btw_focused = true;
            }
            Err(error) => {
                // Error stays until Esc; nothing to scroll, keep prompt focus.
                agent.btw_state = Some(BtwOverlayState::Error { question, error });
                agent.btw_focused = false;
            }
        }
    }
    vec![]
}
