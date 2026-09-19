// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/feedback/upload-trace` extension handler: gate, archive, and one-shot upload of the session trace.
use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::FeedbackTraceUploadIntent;
use agent_client_protocol as acp;
/// Bounds the one-shot GCS upload so a stalled connection can't hang the ACP handler; sized for the 50 MiB archive cap on a slow uplink.
const FEEDBACK_TRACE_UPLOAD_TIMEOUT_SECS: u64 = 120;
pub(super) async fn handle_upload_trace(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    handle_upload_trace_with_session_dir(agent, args, None).await
}
#[cfg(test)]
pub(crate) async fn handle_upload_trace_for_test(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
    session_dir: Option<std::path::PathBuf>,
) -> ExtResult {
    handle_upload_trace_with_session_dir(agent, args, session_dir).await
}
async fn handle_upload_trace_with_session_dir(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
    session_dir_override: Option<std::path::PathBuf>,
) -> ExtResult {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct UploadTraceRequest {
        session_id: String,
        /// Absent on legacy trace-card uploads; an unknown value fails the parse.
        #[serde(default)]
        intent: Option<FeedbackTraceUploadIntent>,
        /// Shell-issued by the successful feedback POST and consumed before archive I/O.
        #[serde(default)]
        trace_upload_token: Option<String>,
    }
    let req: UploadTraceRequest = parse_params(args)?;
    if !agent.cfg.borrow().is_feedback_enabled()
        || agent
            .auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_zdr_team())
        || agent.team_blocks_one_shot_trace_upload()
    {
        return Err(acp::Error::internal_error().data("trace upload is not available"));
    }
    let sid: acp::SessionId = req.session_id.clone().into();
    if agent.resident_handle(&sid).is_none() {
        return Err(
            acp::Error::invalid_params().data(format!("session not found: {}", req.session_id))
        );
    }
    let is_allowed = match req.intent {
        Some(FeedbackTraceUploadIntent::SendThisSession) => req
            .trace_upload_token
            .as_deref()
            .is_some_and(|token| agent.consume_feedback_trace_upload_grant(token, &sid)),
        None => req.trace_upload_token.is_none() && agent.cfg.borrow().is_trace_upload_enabled(),
    };
    if !is_allowed {
        return Err(acp::Error::internal_error().data("trace upload is not available"));
    }
    let session_dir = session_dir_override
        .or_else(|| {
            agent
                .resident_handle(&sid)
                .map(|handle| crate::session::persistence::session_dir(&handle.info))
        })
        .filter(|path| path.is_dir())
        .ok_or_else(|| acp::Error::invalid_params().data("session directory not found"))?;
    let session_id = req.session_id.clone();
    let archive = tokio::task::spawn_blocking({
        let session_dir = session_dir.clone();
        move || distill_feedback::build_session_archive(&session_dir, &session_id)
    })
    .await
    .map_err(|e| acp::Error::internal_error().data(format!("couldn't build session archive: {e}")))?
    .map_err(|e| {
        acp::Error::internal_error().data(format!("couldn't build session archive: {e}"))
    })?;
    let Some(gcs_config) = agent
        .one_shot_feedback_gcs_config(req.session_id.clone())
        .await
    else {
        return Err(acp::Error::internal_error().data("trace upload is not available"));
    };
    let object_path = format!("{}/feedback_trace.tar.gz", req.session_id);
    use crate::upload::gcs::WithAuth as _;
    let auth_manager = Some(agent.auth_manager.clone());
    match tokio::time::timeout(
        std::time::Duration::from_secs(FEEDBACK_TRACE_UPLOAD_TIMEOUT_SECS),
        distill_file_utils::gcs::upload_bytes(
            &gcs_config.with_auth(auth_manager),
            &object_path,
            &archive,
            "application/gzip",
        ),
    )
    .await
    {
        Ok(Ok(_)) => super::to_ext_response(Ok(serde_json::json!({
            "uploaded": true,
            "objectPath": object_path,
        }))),
        Ok(Err(e)) => Err(acp::Error::internal_error().data(format!("trace upload failed: {e:#}"))),
        Err(_) => Err(acp::Error::internal_error().data("trace upload timed out")),
    }
}
