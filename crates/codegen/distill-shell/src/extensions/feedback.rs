// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/feedback`, `x.ai/feedback/dismiss`, `x.ai/btw`, and `x.ai/review/*` extension handlers.
//!
//! - `feedback` and `feedback/dismiss`: persist user ratings and text locally and forward to cli-chat-proxy.
//! - `btw`: dispatch a side question to the active session via `SessionCommand::SideQuestion` and return the answer.
//! - `review/comment` and `review/comment/delete`: record inline code review events to cloud storage.

use std::sync::Arc;

use agent_client_protocol as acp;
use prod_mc_cli_chat_proxy_types::feedback_types::{
    FeedbackImage, FeedbackSubmission, validate_feedback_images,
};

use super::feedback_drafts::feedback_store;
use super::{ExtResult, btw, feedback_drafts, feedback_trace, parse_params, review};
use crate::agent::MvpAgent;
use crate::session::persistence::{LocalFeedbackEntry, UserFeedbackEntry};
use crate::session::{
    ClientFeedbackInput, FeedbackContext, FeedbackDraftSendRequest, FeedbackOutcome,
    FeedbackRequestDismiss, FeedbackResponse, SessionCommand,
};

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/btw" => {
            tracing::info!("handling /btw side question");
            btw::handle_btw(agent, args).await
        }
        "x.ai/feedback" | "x.ai/feedback/dismiss" => {
            tracing::info!("handling user feedback");
            handle_feedback(agent, args).await
        }
        m if m.starts_with(feedback_drafts::DRAFTS_METHOD_PREFIX) => {
            tracing::info!("handling user feedback");
            handle_feedback(agent, args).await
        }
        "x.ai/feedback/upload-trace" => feedback_trace::handle_upload_trace(agent, args).await,
        m if m.starts_with("x.ai/review") => {
            tracing::info!("handling review comment");
            review::handle_review(agent, args).await
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

/// Shown for every `x.ai/feedback*` request while the feature flag is off
pub const FEEDBACK_DISABLED_MESSAGE: &str = "Feedback is disabled. To enable, set \
GROK_FEEDBACK_ENABLED=true or [features] feedback = true in config.toml.";

fn apply_feedback_context(submission: &mut FeedbackSubmission, ctx: Option<FeedbackContext>) {
    let Some(ctx) = ctx else {
        submission.reasoning_effort = None;
        return;
    };
    submission.tool_outcomes = ctx.tool_outcomes;
    submission.session_cwd = Some(ctx.session_cwd);
    submission.compaction_count = Some(ctx.compaction_count);
    submission.context_window_usage = Some(ctx.context_window_usage);
    submission.context_tokens_used = Some(ctx.context_tokens_used);
    submission.context_window_tokens = Some(ctx.context_window_tokens);
    submission.reasoning_effort = ctx.reasoning_effort.map(|effort| effort.to_string());
    if let Some(rated_id) = ctx.model_id {
        if submission.resolved_model_id.as_ref() != Some(&rated_id)
            && submission.model_id.as_ref() != Some(&rated_id)
        {
            submission.model_id = Some(rated_id);
            submission.resolved_model_id = None;
        }
        submission.model_fingerprint = ctx.model_fingerprint;
    }
}

async fn handle_feedback(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if !agent.cfg.borrow().is_feedback_enabled() {
        return Err(acp::Error::internal_error().data(FEEDBACK_DISABLED_MESSAGE));
    }

    match args.method.as_ref() {
        m if m.starts_with(feedback_drafts::DRAFTS_METHOD_PREFIX) => {
            feedback_drafts::handle(agent, args).await
        }
        "x.ai/feedback" => {
            let (mut feedback_input, draft_cleanup) = parse_feedback(agent, args).await?;
            let draft_request = draft_cleanup.is_some();

            let session_id = acp::SessionId::new(feedback_input.session_id.clone());
            let session_handle = agent.resident_handle(&session_id);

            let (model_id, model_metadata) = if let Some(ref session) = session_handle {
                let (tx1, rx1) = tokio::sync::oneshot::channel();
                let _ = session
                    .cmd_tx
                    .send(SessionCommand::GetCurrentModel { responds_to: tx1 });
                let model_id = rx1.await.ok().map(|c| c.id);
                let model_metadata = session.get_model_metadata().await;
                (model_id, model_metadata)
            } else {
                let sampling_config = agent.sampling_config.borrow().clone();
                (Some(sampling_config.model.clone()), Default::default())
            };

            let turn_number = feedback_input.turn_number;

            let mut submission = feedback_input.take_submission(
                model_id,
                model_metadata.resolved_model_id,
                model_metadata.model_fingerprint,
                turn_number,
            );
            let turn_number = submission.turn_number;

            // Enrich with session context for Slack notifications (best-effort).
            let ctx = if let Some(ref session_handle) = session_handle {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = session_handle
                    .cmd_tx
                    .send(SessionCommand::GetFeedbackContext {
                        turn_number,
                        responds_to: tx,
                    });
                rx.await.ok()
            } else {
                None
            };
            apply_feedback_context(&mut submission, ctx);

            // Track rating in session signals
            if let (Some(session_handle), Some(rating_value)) =
                (&session_handle, feedback_input.rating_value)
            {
                use prod_mc_cli_chat_proxy_types::feedback_types::RatingType;
                let (is_positive, is_negative) = match feedback_input.rating_type {
                    // Thumbs: -1 is down, 0 is neutral, 1 is up
                    Some(RatingType::Thumbs) | None => (rating_value > 0, rating_value < 0),
                    // Stars (1-5): >= 4 positive, <= 2 negative, 3 neutral
                    Some(RatingType::Stars) => (rating_value >= 4, rating_value <= 2),
                    // NPS (0-10): 9-10 promoter, 0-6 detractor, 7-8 passive
                    Some(RatingType::Nps) => (rating_value >= 9, rating_value <= 6),
                };
                if is_positive {
                    session_handle.signals_handle.record_positive_rating();
                } else if is_negative {
                    session_handle.signals_handle.record_negative_rating();
                }
            }

            // Log feedback type for debugging
            if feedback_input.is_solicited() {
                tracing::info!(
                    session_id = %feedback_input.session_id,
                    request_id = ?feedback_input.request_id(),
                    turn_number = ?turn_number,
                    "Solicited feedback received (response to feedback request)"
                );
            } else {
                tracing::info!(
                    session_id = %feedback_input.session_id,
                    turn_number = ?turn_number,
                    "Spontaneous user feedback received"
                );
            }

            let telemetry_enabled = agent.product_analytics_enabled();
            let client = agent.feedback_client();
            if client.is_none() {
                tracing::warn!(
                    "no feedback client available (missing proxy credentials); feedback saved locally only"
                );
            }
            // Read the live feedback.user config; the session-actor path uses its spawn-time snapshot
            // Both dedupe through the same process-wide identity cache, so a stable config resolves identically either way
            // Clone out so the RefCell borrow doesn't span an await.
            let user_cfg = agent.cfg.borrow().feedback.user.clone();
            let author_identity =
                crate::util::user_identity::cached_identity(user_cfg.as_ref()).await;
            let outcome = crate::session::feedback_manager::submit_feedback_workflow(
                &mut submission,
                client.as_ref(),
                session_handle.as_ref().map(|h| &h.persistence_tx),
                crate::session::feedback_manager::SubmitFeedbackOptions {
                    solicited: feedback_input.is_solicited(),
                    telemetry_enabled,
                    author_identity,
                },
            )
            .await;

            let response_outcome = match &outcome {
                crate::session::feedback_manager::SubmitOutcome::Submitted => {
                    tracing::info!("feedback submitted to proxy successfully");
                    Some(delete_sent_draft(draft_cleanup).await)
                }
                crate::session::feedback_manager::SubmitOutcome::LocalOnly => {
                    tracing::warn!("feedback saved locally only (no proxy client)");
                    Some(crate::session::FeedbackOutcome::LocalOnly)
                }
                crate::session::feedback_manager::SubmitOutcome::Failed(error) => {
                    tracing::error!(%error, "feedback submission to proxy failed");
                    if draft_cleanup.is_some() && is_feedback_outcome_unknown(error) {
                        Some(crate::session::FeedbackOutcome::OutcomeUnknown)
                    } else {
                        return Err(acp::Error::internal_error()
                            .data(format!("Feedback submission failed: {error}")));
                    }
                }
            };
            let success = !draft_request
                || matches!(
                    response_outcome,
                    Some(
                        crate::session::FeedbackOutcome::Submitted
                            | crate::session::FeedbackOutcome::SubmittedCleanupFailed
                    )
                );
            let trace_upload_token = (feedback_input.request_trace_upload_token
                && matches!(
                    outcome,
                    crate::session::feedback_manager::SubmitOutcome::Submitted
                )
                && agent.feedback_trace_offer())
            .then(|| agent.issue_feedback_trace_upload_grant(session_id));
            let value = serde_json::to_value(FeedbackResponse {
                success,
                outcome: response_outcome,
                trace_upload_token,
            })
            .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
            .expect("to work")
            .expect("to work");
            Ok(acp::ExtResponse::new(value))
        }
        "x.ai/feedback/dismiss" => dismiss_request(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

/// A draft body reaches `parse_draft_send_request` as raw JSON. A legacy body is parsed and validated here.
async fn parse_feedback(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
) -> Result<(ClientFeedbackInput, Option<DraftCleanup>), acp::Error> {
    let params: serde_json::Value = serde_json::from_str(args.params.get())
        .map_err(|error| acp::Error::invalid_params().data(error.to_string()))?;
    if params.get("draft_id").is_some() {
        let request = parse_draft_send_request(params)?;
        let store = feedback_store(agent, &request.session_id)?;
        let (input, cleanup) = draft_send_input(request, store).await?;
        return Ok((input, Some(cleanup)));
    }

    let mut input = parse_legacy_feedback(args)?;
    input.request_trace_upload_token = params
        .get("request_trace_upload_token")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    validate_images(&input.images)?;
    Ok((input, None))
}

/// The store and draft id to delete once the proxy has accepted the report.
pub type DraftCleanup = (
    distill_feedback::FeedbackDraftStore,
    distill_feedback::FeedbackDraftId,
);

fn parse_legacy_feedback(args: &acp::ExtRequest) -> Result<ClientFeedbackInput, acp::Error> {
    match serde_json::from_str::<ClientFeedbackInput>(args.params.get()) {
        Ok(input) => Ok(input),
        Err(_) => {
            let simple: crate::session::FeedbackRequest = parse_params(args)?;
            Ok(ClientFeedbackInput {
                session_id: simple.session_id,
                client_type: prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
                rating_type: None,
                rating_value: None,
                feedback_text: Some(simple.feedback_text),
                images: vec![],
                feedback_categories: vec![],
                context_type: None,
                turn_number: None,
                request_id: None,
                client_version: None,
                metadata: None,
                terminal_info: None,
                request_trace_upload_token: false,
            })
        }
    }
}

fn validate_images(images: &[FeedbackImage]) -> Result<(), acp::Error> {
    validate_feedback_images(images)
        .map_err(|error| acp::Error::invalid_params().data(format!("feedback images: {error}")))
}

/// Parses a draft send request. Form fields outside `edited_body` are rejected.
pub fn parse_draft_send_request(
    params: serde_json::Value,
) -> Result<FeedbackDraftSendRequest, acp::Error> {
    let object = params
        .as_object()
        .ok_or_else(|| acp::Error::invalid_params().data("feedback params must be an object"))?;
    for legacy in [
        "feedback_text",
        "images",
        "rating_type",
        "rating_value",
        "feedback_categories",
        "context_type",
        "turn_number",
        "request_id",
        "metadata",
        "type",
        "task_category",
        "failure_mode",
    ] {
        if object.contains_key(legacy) {
            return Err(acp::Error::invalid_params().data(format!(
                "draft feedback field `{legacy}` belongs in edited_body"
            )));
        }
    }

    serde_json::from_value(params)
        .map_err(|error| acp::Error::invalid_params().data(error.to_string()))
}

/// Builds the report for a draft send. Fails when `store` no longer holds the draft.
pub async fn draft_send_input(
    request: FeedbackDraftSendRequest,
    store: distill_feedback::FeedbackDraftStore,
) -> Result<(ClientFeedbackInput, DraftCleanup), acp::Error> {
    let draft_id = request.draft_id;
    let lookup_store = store.clone();
    let lookup_id = draft_id.clone();
    let is_present = tokio::task::spawn_blocking(move || lookup_store.get(&lookup_id))
        .await
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?
        .is_some();
    if !is_present {
        return Err(acp::Error::invalid_params().data("feedback draft not found"));
    }

    let body = request.edited_body;
    let taxonomy = distill_feedback::FeedbackTaxonomy {
        r#type: Some(body.input.r#type),
        task_category: body.input.task_category,
        failure_mode: body.input.failure_mode,
    };
    distill_feedback::validate_feedback_draft_send(&body.input, !body.images.is_empty())
        .map_err(|error| acp::Error::invalid_params().data(error.to_string()))?;
    validate_images(&body.images)?;
    let metadata = Some(distill_feedback::structured_feedback(
        distill_feedback::FeedbackSource::Draft,
        taxonomy,
    ));
    Ok((
        ClientFeedbackInput {
            session_id: request.session_id,
            client_type: prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
            rating_type: None,
            rating_value: None,
            feedback_text: Some(distill_feedback::post_text(
                &body.input.title,
                &body.input.details,
            )),
            images: body.images,
            feedback_categories: vec![],
            context_type: None,
            turn_number: None,
            request_id: None,
            client_version: body.client_version,
            metadata,
            terminal_info: body.terminal_info,
            request_trace_upload_token: request.request_trace_upload_token,
        },
        (store, draft_id),
    ))
}

/// The report has already been sent. A draft left behind can only produce a duplicate report.
pub async fn delete_sent_draft(cleanup: Option<DraftCleanup>) -> FeedbackOutcome {
    let Some((store, draft_id)) = cleanup else {
        return FeedbackOutcome::Submitted;
    };

    let deleted = tokio::task::spawn_blocking(move || store.delete(&draft_id))
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result.map_err(|error| error.to_string()));
    match deleted {
        Ok(_) => FeedbackOutcome::Submitted,
        Err(error) => {
            tracing::warn!(%error, "sent draft not deleted");
            FeedbackOutcome::SubmittedCleanupFailed
        }
    }
}

fn is_feedback_outcome_unknown(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(request_may_have_arrived)
    })
}

/// False only when the request never left (bad URL, connection refused) or the proxy answered.
/// A retry after false cannot send a duplicate.
pub fn request_may_have_arrived(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_connect() && !error.is_status()
}

/// Tells the proxy that the user dismissed a solicited feedback request.
async fn dismiss_request(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let input: FeedbackRequestDismiss = parse_params(args)?;
    tracing::info!(
        session_id = %input.session_id,
        request_id = %input.request_id,
        "Feedback request dismissed by user"
    );

    // `submit_feedback_workflow` checks the same flag before its "responded" event
    if agent.product_analytics_enabled() {
        distill_telemetry::event_span!(
            "feedback.survey",
            survey_type = "session",
            event_type = "dismissed",
            appearance_id = %input.request_id,
            has_feedback_text = false,
            is_solicited = true,
        );
    }

    persist_dismissal(agent, &input);

    let client = agent
        .feedback_client()
        .ok_or_else(|| acp::Error::internal_error().data("No credentials for feedback"))?;
    let feedback_url = agent.cfg.borrow().endpoints.resolve_feedback_base_url();
    match client.dismiss_request(&input.request_id).await {
        Ok(response) => {
            tracing::info!(
                request_id = %response.request_id,
                status = %response.status,
                %feedback_url,
                "Feedback request dismissed"
            );
            super::to_raw_response(&response)
        }
        Err(error) => {
            tracing::warn!(
                %error,
                request_id = %input.request_id,
                %feedback_url,
                "Failed to dismiss feedback request"
            );
            Err(acp::Error::internal_error()
                .data(format!("Failed to dismiss feedback request: {error}")))
        }
    }
}

/// The persistence actor flushes this entry before the storage `CopyFile`.
fn persist_dismissal(agent: &MvpAgent, input: &FeedbackRequestDismiss) {
    let session_id = acp::SessionId::new(input.session_id.clone());
    if let Some(session_handle) = agent.resident_handle(&session_id) {
        session_handle.persist_feedback(LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: input.session_id.clone(),
            turn_number: None,
            solicited: true,
            request_id: Some(input.request_id.clone()),
            dismissed: true,
            submission: None,
        }));
    }
}

#[cfg(test)]
#[path = "feedback_tests.rs"]
mod tests;
