// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/review/*` extension handlers: record inline code review events to cloud storage.

use std::sync::Arc;

use agent_client_protocol as acp;

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::{
    CommentDeleteRequest, CommentDeleteResponse, CommentRequest, CommentResponse,
};
use crate::upload::gcs::WithAuth as _;
use distill_file_utils::gcs::upload_bytes;
use distill_telemetry::id::agent_id;

/// Record inline code review events.
/// Methods: `x.ai/review/comment`: record a new inline code comment to cloud storage `x.ai/review/comment/delete`: record a tombstone event for a deleted comment
pub(super) async fn handle_review(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/review/comment" => {
            let request: CommentRequest = parse_params(args)?;

            let comment_id = uuid::Uuid::now_v7().to_string();

            tracing::info!(
                comment_id = %comment_id,
                session_id = %request.session_id,
                prompt_index = request.prompt_index,
                path = %request.citation.path,
                lines = %format!("{}-{}", request.citation.start_line, request.citation.end_line),
                "Comment received"
            );

            let record = serde_json::json!({
                "event": "create",
                "commentId": comment_id,
                "sessionId": request.session_id,
                "promptIndex": request.prompt_index,
                "comment": null,
                "citation": request.citation,
                "agentId": agent_id().to_string(),
                "clientType": format!("{:?}", agent.client_type()),
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });

            if let Some(gcs_config) = agent
                .build_gcs_config(format!("{}/comments", request.session_id))
                .await
            {
                let json_bytes = serde_json::to_vec_pretty(&record)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                let gcs_path = format!(
                    "{}/{}.json",
                    gcs_config.gcs_prefix.as_deref().unwrap_or("comments"),
                    comment_id
                );

                let auth_manager = Some(agent.auth_manager.clone());
                tokio::spawn(async move {
                    match upload_bytes(
                        &gcs_config.with_auth(auth_manager),
                        &gcs_path,
                        &json_bytes,
                        "application/json",
                    )
                    .await
                    {
                        Ok(gcs_url) => {
                            tracing::info!(gcs_url = %gcs_url, "Comment uploaded to GCS");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, gcs_path, "Failed to upload comment to GCS");
                        }
                    }
                });
            }

            let value = serde_json::to_value(CommentResponse {
                comment_id,
                recorded: true,
            })
            .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
            .expect("to work")
            .expect("to work");
            Ok(acp::ExtResponse::new(value))
        }
        "x.ai/review/comment/delete" => {
            let request: CommentDeleteRequest = parse_params(args)?;

            tracing::info!(
                comment_id = %request.comment_id,
                session_id = %request.session_id,
                "Comment delete received"
            );

            let record = serde_json::json!({
                "event": "delete",
                "commentId": request.comment_id,
                "sessionId": request.session_id,
                "agentId": agent_id().to_string(),
                "clientType": format!("{:?}", agent.client_type()),
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });

            if let Some(gcs_config) = agent
                .build_gcs_config(format!("{}/comments", request.session_id))
                .await
            {
                let json_bytes = serde_json::to_vec_pretty(&record)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                let event_id = uuid::Uuid::now_v7().to_string();
                let gcs_path = format!(
                    "{}/{}.json",
                    gcs_config.gcs_prefix.as_deref().unwrap_or("comments"),
                    event_id
                );

                let auth_manager = Some(agent.auth_manager.clone());
                tokio::spawn(async move {
                    match upload_bytes(
                        &gcs_config.with_auth(auth_manager),
                        &gcs_path,
                        &json_bytes,
                        "application/json",
                    )
                    .await
                    {
                        Ok(gcs_url) => {
                            tracing::info!(gcs_url = %gcs_url, "Comment delete event uploaded to GCS");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, gcs_path, "Failed to upload comment delete event to GCS");
                        }
                    }
                });
            }

            let value = serde_json::to_value(CommentDeleteResponse {
                comment_id: request.comment_id,
                deleted: true,
            })
            .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
            .expect("to work")
            .expect("to work");
            Ok(acp::ExtResponse::new(value))
        }
        _ => Err(acp::Error::method_not_found()),
    }
}
