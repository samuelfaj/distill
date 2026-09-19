// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/feedback/drafts/*` extension handlers over a session's `FeedbackDraftStore`.

use agent_client_protocol as acp;
use distill_feedback::{DeleteOutcome, FeedbackDraftStore, FeedbackStoreError, UpdateOutcome};
use distill_telemetry::events::{FeedbackDraftOp, FeedbackDraftOpError, FeedbackDraftOpKind};
use distill_telemetry::session_ctx::log_event_dual;

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::FeedbackDraftUpdateRequest;

pub const DRAFTS_METHOD_PREFIX: &str = "x.ai/feedback/drafts/";

#[derive(serde::Deserialize)]
struct FeedbackDraftSessionRequest {
    session_id: String,
}

pub(super) async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let store = feedback_store(agent, &requested_session_id(args)?)?;
    answer(args, store, agent.product_analytics_enabled()).await
}

/// Answers one `x.ai/feedback/drafts/*` request against the given store.
pub async fn answer(
    args: &acp::ExtRequest,
    store: FeedbackDraftStore,
    telemetry_enabled: bool,
) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/feedback/drafts/list" => list_feedback_drafts(args, store, telemetry_enabled).await,
        "x.ai/feedback/drafts/get" => get_feedback_draft(args, store, telemetry_enabled).await,
        "x.ai/feedback/drafts/delete" => {
            delete_feedback_draft(args, store, telemetry_enabled).await
        }
        "x.ai/feedback/drafts/update" => {
            update_feedback_draft(args, store, telemetry_enabled).await
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

/// The `session_id` a drafts request names.
/// Callers pick the store from it before calling `answer`.
pub fn requested_session_id(args: &acp::ExtRequest) -> Result<String, acp::Error> {
    parse_params::<FeedbackDraftSessionRequest>(args).map(|request| request.session_id)
}

#[derive(serde::Deserialize)]
struct FeedbackDraftRequest {
    session_id: String,
    draft_id: distill_feedback::FeedbackDraftId,
}

pub(super) fn feedback_store(
    agent: &MvpAgent,
    session_id: &str,
) -> Result<distill_feedback::FeedbackDraftStore, acp::Error> {
    let session_id = acp::SessionId::new(session_id.to_owned());
    let handle = agent.resident_handle(&session_id).ok_or_else(|| {
        acp::Error::invalid_params().data(format!("session not found: {session_id}"))
    })?;
    Ok(distill_feedback::FeedbackDraftStore::new(
        crate::session::persistence::session_dir(&handle.info),
    ))
}

/// Variant-only: `Display` embeds the session path. Exhaustive so a new variant is a compile error.
pub fn draft_op_error(error: &FeedbackStoreError) -> FeedbackDraftOpError {
    match error {
        FeedbackStoreError::Busy => FeedbackDraftOpError::Busy,
        FeedbackStoreError::DraftNotFound { .. } => FeedbackDraftOpError::NotFound,
        FeedbackStoreError::Decode { .. }
        | FeedbackStoreError::UnsupportedSchema { .. }
        | FeedbackStoreError::InvalidDocument { .. }
        | FeedbackStoreError::EmptyDraftId
        | FeedbackStoreError::DuplicateDraftId { .. }
        | FeedbackStoreError::InvalidRevision { .. }
        | FeedbackStoreError::TooLarge { .. }
        | FeedbackStoreError::DraftCapacityExceeded { .. } => FeedbackDraftOpError::InvalidDocument,
        FeedbackStoreError::InvalidSessionDirectory { .. }
        | FeedbackStoreError::Inspect { .. }
        | FeedbackStoreError::OpenLock { .. }
        | FeedbackStoreError::Lock(_)
        | FeedbackStoreError::Read { .. }
        | FeedbackStoreError::SymlinkPath { .. }
        | FeedbackStoreError::NonFilePath { .. }
        | FeedbackStoreError::CreateTemp { .. }
        | FeedbackStoreError::Write { .. }
        | FeedbackStoreError::Persist { .. }
        | FeedbackStoreError::SyncDirectory { .. } => FeedbackDraftOpError::Io,
        FeedbackStoreError::Encode { .. }
        | FeedbackStoreError::BlankTitle
        | FeedbackStoreError::BlankDetails
        | FeedbackStoreError::TitleTooLarge { .. }
        | FeedbackStoreError::DetailsTooLarge { .. }
        | FeedbackStoreError::AreaTooLarge { .. }
        | FeedbackStoreError::ClockBeforeUnixEpoch(_)
        | FeedbackStoreError::ClockOutOfRange => FeedbackDraftOpError::Other,
    }
}

/// `Ok` carries the `list` row count; every other op reports `Ok(None)` on success.
type DraftOpOutcome = Result<Option<u32>, FeedbackDraftOpError>;

pub(super) fn draft_op_event(
    session_id: &str,
    op: FeedbackDraftOpKind,
    outcome: DraftOpOutcome,
) -> FeedbackDraftOp {
    FeedbackDraftOp {
        session_id: session_id.to_owned(),
        op,
        ok: outcome.is_ok(),
        error: outcome.err(),
        draft_count: outcome.ok().flatten(),
        skipped: None,
    }
}

/// Runs one store op and builds the response before it emits `feedback_draft_op`, so telemetry
/// cannot alter what the client gets.
async fn draft_op<T: Send + 'static>(
    telemetry_enabled: bool,
    session_id: &str,
    op: FeedbackDraftOpKind,
    store_op: impl FnOnce() -> distill_feedback::Result<T> + Send + 'static,
    respond: impl FnOnce(T) -> (ExtResult, DraftOpOutcome),
) -> ExtResult {
    let result = tokio::task::spawn_blocking(store_op)
        .await
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
    let (response, outcome) = match result {
        Ok(value) => respond(value),
        Err(error) => (
            Err(acp::Error::internal_error().data(error.to_string())),
            Err(draft_op_error(&error)),
        ),
    };
    log_event_dual(telemetry_enabled, draft_op_event(session_id, op, outcome));
    response
}

async fn list_feedback_drafts(
    args: &acp::ExtRequest,
    store: FeedbackDraftStore,
    telemetry_enabled: bool,
) -> ExtResult {
    let session_id = requested_session_id(args)?;
    draft_op(
        telemetry_enabled,
        &session_id,
        FeedbackDraftOpKind::List,
        move || store.list(),
        |drafts| {
            (
                super::to_raw_response(&serde_json::json!({ "drafts": drafts })),
                Ok(Some(drafts.len() as u32)),
            )
        },
    )
    .await
}

async fn get_feedback_draft(
    args: &acp::ExtRequest,
    store: FeedbackDraftStore,
    telemetry_enabled: bool,
) -> ExtResult {
    let request: FeedbackDraftRequest = parse_params(args)?;
    let draft_id = request.draft_id;
    draft_op(
        telemetry_enabled,
        &request.session_id,
        FeedbackDraftOpKind::Load,
        move || store.get(&draft_id),
        |draft| match draft {
            Some(draft) => (
                super::to_raw_response(&serde_json::json!({ "draft": draft })),
                Ok(None),
            ),
            None => (
                Err(acp::Error::invalid_params().data("feedback draft not found")),
                Err(FeedbackDraftOpError::NotFound),
            ),
        },
    )
    .await
}

async fn update_feedback_draft(
    args: &acp::ExtRequest,
    store: FeedbackDraftStore,
    telemetry_enabled: bool,
) -> ExtResult {
    let request: FeedbackDraftUpdateRequest = parse_params(args)?;
    draft_op(
        telemetry_enabled,
        &request.session_id,
        FeedbackDraftOpKind::Recover,
        move || store.update_from_input(&request.draft_id, request.input),
        |updated| {
            (
                super::to_raw_response(&serde_json::json!({
                    "updated": matches!(updated, UpdateOutcome::Updated),
                })),
                match updated {
                    UpdateOutcome::Updated => Ok(None),
                    UpdateOutcome::NotFound => Err(FeedbackDraftOpError::NotFound),
                },
            )
        },
    )
    .await
}

async fn delete_feedback_draft(
    args: &acp::ExtRequest,
    store: FeedbackDraftStore,
    telemetry_enabled: bool,
) -> ExtResult {
    let request: FeedbackDraftRequest = parse_params(args)?;
    let draft_id = request.draft_id;
    draft_op(
        telemetry_enabled,
        &request.session_id,
        FeedbackDraftOpKind::Delete,
        move || store.delete(&draft_id),
        |deleted| {
            (
                super::to_raw_response(&serde_json::json!({
                    "deleted": matches!(deleted, DeleteOutcome::Deleted),
                })),
                match deleted {
                    DeleteOutcome::Deleted => Ok(None),
                    DeleteOutcome::NotFound => Err(FeedbackDraftOpError::NotFound),
                },
            )
        },
    )
    .await
}
