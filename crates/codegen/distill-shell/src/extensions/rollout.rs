// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/rollout/survey` extension handler.
//!
//! Logs a rollout-survey submission via telemetry (Mixpanel + BigQuery).

use agent_client_protocol as acp;

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::session::{RolloutSurveyRequest, RolloutSurveyResponse};
use distill_telemetry::events::RolloutSurvey;
use distill_telemetry::session_ctx::log_event;

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(_agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/rollout/survey" => {
            let req: RolloutSurveyRequest = parse_params(args)?;

            distill_telemetry::event_span!(
                "feedback.survey",
                survey_type = "rollout",
                event_type = "responded",
                has_feedback_text = !req.feedback.is_empty(),
                preference_count = req.preferences.len() as i64,
            );

            log_event(RolloutSurvey {
                session_id: req.session_id.clone(),
                preferences: req.preferences.clone(),
                has_feedback: !req.feedback.is_empty(),
            });

            tracing::info!(
                "Rollout survey received for session {}: preferences={:?}, feedback={}",
                req.session_id,
                req.preferences,
                req.feedback,
            );

            to_raw_response(&RolloutSurveyResponse { success: true })
        }
        _ => Err(acp::Error::method_not_found()),
    }
}
